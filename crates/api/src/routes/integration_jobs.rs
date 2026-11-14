// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Handing a job the operator already has to a download client (ADR 0016,
//! docs/plans/acquisition.md A4 and A5): a torrent or magnet link to
//! qBittorrent, an NZB to SABnzbd or NZBGet. Tangible names the job itself (a
//! torrent's infohash, an NZB's SHA-256) before the client is asked
//! anything, so a job submitted again is found rather than doubled, adds it
//! to the integration's category with nothing else (no save path, rename or
//! script), and records the import that follows it.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tangible_db::integrations::{
    NewIntegrationJob, Submitted, get_integration, standing_integration_job, submit_integration_job,
};
use tangible_integrations::nzb::{MAX_NZB_BYTES, Nzb};
use tangible_integrations::qbittorrent::{Added, TorrentSource};
use tangible_integrations::torrent::{
    Infohash, MAX_TORRENT_BYTES, infohash_of_magnet, infohash_of_torrent,
};
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::download_clients::{DownloadClient, Folders, open_client};
use crate::problem::{ErrorCode, Problem};
use crate::routes::integrations::{
    client_problem, note_health, open_problem, parse_id, unavailable,
};
use crate::state::ApiState;

/// The largest body a submission may have: the larger of a `.torrent` and
/// an NZB at its limit, written base64, with room for the JSON around it.
pub const MAX_SUBMISSION_BYTES: usize = if MAX_NZB_BYTES > MAX_TORRENT_BYTES {
    MAX_NZB_BYTES
} else {
    MAX_TORRENT_BYTES
} / 3
    * 4
    + 4096;

/// A job the operator already has, for a download client to fetch: a
/// magnet link or `.torrent` for qBittorrent, an NZB for SABnzbd. Exactly
/// one.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitJobRequest {
    /// A magnet link.
    #[serde(default)]
    pub magnet: Option<String>,
    /// A `.torrent` file, base64.
    #[serde(default)]
    pub torrent: Option<String>,
    /// An NZB file, base64.
    #[serde(default)]
    pub nzb: Option<String>,
}

/// A job handed to a download client, and the import following it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrationJobView {
    /// The integration.
    pub integration_id: String,
    /// How Tangible names the job: a torrent's infohash, or an NZB's
    /// SHA-256. Submitting the same again finds this job.
    pub identity: String,
    /// The client's own id for the job (the infohash, a SABnzbd `nzo_id`),
    /// when it was handed over now.
    pub client_job_id: Option<String>,
    /// The import that brings it into the library when it finishes.
    pub import_id: String,
    /// `created`; `resubmitted` when an earlier import of it ended without
    /// an artifact; `already_active` when one is still going;
    /// `already_imported` when it is in the library.
    pub outcome: String,
    /// What it became, when already imported.
    pub artifact_id: Option<String>,
}

/// A submission, checked, with the bytes it carries.
pub(crate) enum Submission {
    Magnet(String, Infohash),
    Torrent(Vec<u8>, Infohash),
    Nzb(Vec<u8>, Nzb),
}

fn refuse(detail: impl Into<String>) -> Problem {
    Problem::new(ErrorCode::ValidationFailed, detail)
}

fn base64(text: &str, what: &str) -> Result<Vec<u8>, Problem> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|_| refuse(format!("{what} must be the file, base64")))
}

impl Submission {
    /// A magnet link, checked.
    pub(crate) fn magnet(link: &str) -> Result<Self, String> {
        let link = link.trim();
        let hash = infohash_of_magnet(link).map_err(|error| error.to_string())?;
        Ok(Self::Magnet(link.to_owned(), hash))
    }

    /// A `.torrent` file, checked.
    pub(crate) fn torrent(bytes: Vec<u8>) -> Result<Self, String> {
        let hash = infohash_of_torrent(&bytes).map_err(|error| error.to_string())?;
        Ok(Self::Torrent(bytes, hash))
    }

    /// An NZB, checked.
    pub(crate) fn nzb(bytes: Vec<u8>) -> Result<Self, String> {
        let nzb = Nzb::check(&bytes).map_err(|error| error.to_string())?;
        Ok(Self::Nzb(bytes, nzb))
    }

    /// The submission a request makes for `client`, or why not.
    fn parse(request: &SubmitJobRequest, client: &DownloadClient) -> Result<Self, Problem> {
        let given = [&request.magnet, &request.torrent, &request.nzb]
            .iter()
            .filter(|field| field.is_some())
            .count();
        if given != 1 {
            return Err(refuse(match client {
                DownloadClient::Qbittorrent(_) => "send either magnet or torrent, and nothing else",
                DownloadClient::Sabnzbd(_) | DownloadClient::Nzbget(_) => {
                    "send an nzb, and nothing else"
                }
            }));
        }
        match (client, request) {
            (
                DownloadClient::Qbittorrent(_),
                SubmitJobRequest {
                    magnet: Some(magnet),
                    ..
                },
            ) => Self::magnet(magnet).map_err(refuse),
            (
                DownloadClient::Qbittorrent(_),
                SubmitJobRequest {
                    torrent: Some(text),
                    ..
                },
            ) => Self::torrent(base64(text, "torrent")?).map_err(refuse),
            (
                DownloadClient::Sabnzbd(_) | DownloadClient::Nzbget(_),
                SubmitJobRequest {
                    nzb: Some(text), ..
                },
            ) => Self::nzb(base64(text, "nzb")?).map_err(refuse),
            (DownloadClient::Qbittorrent(_), _) => Err(refuse(
                "a qBittorrent takes a magnet link or a .torrent file, not an NZB",
            )),
            (DownloadClient::Sabnzbd(_) | DownloadClient::Nzbget(_), _) => Err(refuse(format!(
                "{} takes an NZB, not a magnet link or a .torrent file",
                client.name()
            ))),
        }
    }

    /// How Tangible names the job.
    fn identity(&self) -> String {
        match self {
            Self::Magnet(_, hash) | Self::Torrent(_, hash) => hash.to_string(),
            Self::Nzb(_, nzb) => nzb.sha256().to_owned(),
        }
    }
}

/// Add the job to the client in its category, answering the client's own id
/// for it. A torrent the client declines must already be there, in that
/// category; an NZB's category must exist in SABnzbd, which would otherwise
/// file it under its default.
async fn hand_over(
    state: &ApiState,
    id: uuid::Uuid,
    client: &mut DownloadClient,
    folders: &Folders,
    submission: &Submission,
) -> Result<String, Problem> {
    let failed = |error: tangible_integrations::download::ClientError| async move {
        note_health(state, id, Err(error.code())).await;
        client_problem(&error)
    };
    match (client, submission) {
        (
            DownloadClient::Qbittorrent(client),
            Submission::Magnet(_, hash) | Submission::Torrent(_, hash),
        ) => {
            let source = match submission {
                Submission::Magnet(link, _) => TorrentSource::Magnet(link),
                Submission::Torrent(bytes, _) | Submission::Nzb(bytes, _) => {
                    TorrentSource::File(bytes)
                }
            };
            let added = match client.add(&source, &folders.category).await {
                Ok(added) => added,
                Err(error) => return Err(failed(error).await),
            };
            if added == Added::Declined {
                match client.torrent(hash).await {
                    Ok(Some(torrent)) if torrent.category != folders.category => {
                        return Err(Problem::new(
                            ErrorCode::Conflict,
                            "qBittorrent already has this torrent under another category; \
                             Tangible follows only torrents in its own, so move it there in \
                             qBittorrent first",
                        ));
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        return Err(refuse(
                            "qBittorrent declined the torrent, and does not have it",
                        ));
                    }
                    Err(error) => return Err(failed(error).await),
                }
            }
            Ok(hash.to_string())
        }
        (DownloadClient::Sabnzbd(client), Submission::Nzb(bytes, nzb)) => {
            let categories = match client.categories().await {
                Ok(categories) => categories,
                Err(error) => return Err(failed(error).await),
            };
            if !categories.contains(&folders.category) {
                note_health(state, id, Err("INTEGRATION_MISCONFIGURED")).await;
                return Err(Problem::new(
                    ErrorCode::IntegrationMisconfigured,
                    format!(
                        "SABnzbd has no category {}; add it under Config, Categories, with its \
                         own folder, or SABnzbd files the download under its default",
                        folders.category
                    ),
                ));
            }
            let filename = format!("{}.nzb", nzb.job_name());
            match client.add(bytes, &filename, &folders.category).await {
                Ok(Some(nzo_id)) => Ok(nzo_id),
                Ok(None) => Err(refuse("SABnzbd could not read the NZB")),
                Err(error) => Err(failed(error).await),
            }
        }
        (DownloadClient::Nzbget(client), Submission::Nzb(bytes, nzb)) => {
            let categories = match client.categories().await {
                Ok(categories) => categories,
                Err(error) => return Err(failed(error).await),
            };
            // NZBGet matches category names regardless of case.
            if !categories
                .iter()
                .any(|category| category.eq_ignore_ascii_case(&folders.category))
            {
                note_health(state, id, Err("INTEGRATION_MISCONFIGURED")).await;
                return Err(Problem::new(
                    ErrorCode::IntegrationMisconfigured,
                    format!(
                        "NZBGet has no category {}; add it under Settings, Categories, or NZBGet \
                         files the download under that name, outside the folder a category of \
                         its own would have",
                        folders.category
                    ),
                ));
            }
            let filename = format!("{}.nzb", nzb.job_name());
            match client.append(bytes, &filename, &folders.category).await {
                Ok(Some(nzbid)) => Ok(nzbid.to_string()),
                Ok(None) => Err(refuse("NZBGet refused the NZB")),
                Err(error) => Err(failed(error).await),
            }
        }
        _ => Err(refuse("that does not suit this client")),
    }
}

fn respond(
    id: uuid::Uuid,
    identity: String,
    client_job_id: Option<String>,
    submitted: Submitted,
) -> axum::response::Response {
    let (status, outcome, import_id, artifact_id) = match submitted {
        Submitted::Created { import_id } => (StatusCode::ACCEPTED, "created", import_id, None),
        Submitted::Resubmitted { import_id } => {
            (StatusCode::ACCEPTED, "resubmitted", import_id, None)
        }
        Submitted::AlreadyActive { import_id } => {
            (StatusCode::OK, "already_active", import_id, None)
        }
        Submitted::AlreadyImported {
            import_id,
            artifact_id,
        } => (StatusCode::OK, "already_imported", import_id, artifact_id),
    };
    (
        status,
        Json(IntegrationJobView {
            integration_id: id.to_string(),
            identity,
            client_job_id,
            import_id: import_id.to_string(),
            outcome: outcome.to_owned(),
            artifact_id: artifact_id.map(|id| id.to_string()),
        }),
    )
        .into_response()
}

/// Hand a job to a download client.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a request or file that will not do, or one that
/// does not suit the client; `NOT_FOUND`; `INTEGRATION_OFF`;
/// `INTEGRATION_MISCONFIGURED` for a SABnzbd without the category;
/// `CONFLICT` when qBittorrent holds the torrent under another category; the
/// client's failures as `INTEGRATION_*`.
#[utoipa::path(
    post,
    path = "/api/v1/integrations/{integration_id}/jobs",
    tag = "integrations",
    description = "Hand a job the operator already has to a download client, in its \\
                   category: a magnet link or a .torrent file (base64) to qBittorrent, an NZB \\
                   (base64) to SABnzbd. An import follows it and brings it into the library \\
                   when it finishes. No script is asked for, and the job and its data are \\
                   removed only once imported, where the integration is set to remove them. \\
                   Submitting the same again is safe: the existing import is answered, and the \\
                   client is not asked twice.",
    params(("integration_id" = String, Path, description = "Integration identifier")),
    request_body = SubmitJobRequest,
    responses(
        (status = 202, description = "Handed over; an import follows it", body = IntegrationJobView),
        (status = 200, description = "Handed over before", body = IntegrationJobView),
        (status = 404, description = "No such integration", body = Problem),
        (status = 409, description = "Turned off, misconfigured, or held under another category", body = Problem),
        (status = 422, description = "Not a usable file or link for this client", body = Problem),
        (status = 502, description = "The client failed", body = Problem),
    ),
)]
pub async fn submit_job(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(integration_id): Path<String>,
    Json(request): Json<SubmitJobRequest>,
) -> Result<axum::response::Response, Problem> {
    let id = parse_id(&integration_id)?;
    let record = get_integration(state.database().pool(), id)
        .await
        .map_err(unavailable("the integration could not be read"))?
        .ok_or_else(|| Problem::not_found("integration", &id.to_string()))?;
    let (mut client, folders) = open_client(
        state.database(),
        state.integration_client(),
        state.secret_key(),
        &record,
    )
    .await
    .map_err(|error| open_problem(&error))?;
    let submission = Submission::parse(&request, &client)?;
    let delivered = deliver(
        &state,
        id,
        &mut client,
        &folders,
        &submission,
        &user.username,
    )
    .await?;
    Ok(respond(
        id,
        delivered.identity,
        delivered.client_job_id,
        delivered.submitted,
    ))
}

/// A job handed over, or found handed over before.
pub(crate) struct Delivered {
    /// How Tangible names the job.
    pub identity: String,
    /// The client's own id for it, when it was handed over now.
    pub client_job_id: Option<String>,
    /// The import following it, and how.
    pub submitted: Submitted,
}

/// Hand `submission` to the download client `id`, opened as `client`, in
/// its category, and record the import that follows it: what
/// `POST /integrations/{id}/jobs` does, and a request's grab (ADR 0021). A
/// job handed over before is found, and the client is not asked again.
pub(crate) async fn deliver(
    state: &ApiState,
    id: uuid::Uuid,
    client: &mut DownloadClient,
    folders: &Folders,
    submission: &Submission,
    actor: &str,
) -> Result<Delivered, Problem> {
    let pool = state.database().pool();
    let identity = submission.identity();

    // Found before the client is asked: SABnzbd would add it again.
    if let Some(standing) = standing_integration_job(pool, id, &identity)
        .await
        .map_err(unavailable("earlier submissions could not be read"))?
    {
        return Ok(Delivered {
            identity,
            client_job_id: None,
            submitted: standing,
        });
    }

    let client_job_id = hand_over(state, id, client, folders, submission).await?;
    note_health(state, id, Ok(None)).await;
    let prefix = match client {
        DownloadClient::Qbittorrent(_) => "qbittorrent",
        DownloadClient::Sabnzbd(_) => "sabnzbd",
        DownloadClient::Nzbget(_) => "nzbget",
    };
    let descriptor = serde_json::json!({
        "integration_id": id,
        "client_job_id": client_job_id,
        "identity": identity,
        "reference": format!("{prefix}:{client_job_id}"),
    });
    let submitted = submit_integration_job(
        pool,
        &NewIntegrationJob {
            integration_id: id,
            client_job_id: &identity,
            source_kind: client.source_kind(),
            descriptor: &descriptor,
            actor,
        },
    )
    .await
    .map_err(unavailable("the job's import could not be recorded"))?;
    tracing::info!(
        integration_id = %id,
        client = prefix,
        client_job_id = %client_job_id,
        "job handed to a download client"
    );
    Ok(Delivered {
        identity,
        client_job_id: Some(client_job_id),
        submitted,
    })
}
