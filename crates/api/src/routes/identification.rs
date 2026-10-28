// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! An artifact's preservation hashes, and what they identify: the games of
//! loaded DATs it matches (docs/plans/dat-matching.md).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tangible_db::dats::{MatchRecord, matches_for};
use tangible_db::identification::{artifact_hashes, request_hashes};
use tangible_domain::ArtifactId;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

fn unavailable(what: &str) -> Problem {
    Problem::new(
        ErrorCode::StorageUnavailable,
        format!("{what}; see server logs"),
    )
}

fn parse_artifact(raw: &str) -> Result<ArtifactId, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("artifact_id", "not a valid identifier"))
}

/// CRC32, MD5 and SHA-1 of some bytes, as preservation databases list them.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HashesView {
    /// Bytes covered.
    pub length_bytes: i64,
    /// CRC32, eight lowercase hex digits.
    pub crc32: Option<String>,
    /// MD5.
    pub md5: Option<String>,
    /// SHA-1.
    pub sha1: Option<String>,
}

/// One component's hashes.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComponentHashesView {
    /// Its path in the artifact.
    pub logical_path: String,
    /// Its hashes, once computed.
    #[serde(flatten)]
    pub hashes: HashesView,
}

/// One CD track's hashes: of its bytes from its first in the file (its
/// pregap included, when the file holds it), as Redump cuts a track.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TrackHashesView {
    /// The track.
    pub number: i32,
    /// Its hashes.
    #[serde(flatten)]
    pub hashes: HashesView,
}

/// One file of a matched game.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DatMatchFileView {
    /// Its name in the DAT.
    pub name: String,
    /// Its size in the DAT.
    pub size: Option<i64>,
    /// The artifact's track (`track 2`) or file with its SHA-1 and size,
    /// or null when none has.
    pub matched_by: Option<String>,
}

/// A game in a loaded DAT that the artifact is, or partly is.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DatMatchView {
    /// The DAT.
    pub dat_id: String,
    /// Its name, such as `Sony - PlayStation`.
    pub dat_name: String,
    /// Its version.
    pub dat_version: Option<String>,
    /// The game's name, such as `Game (USA)`.
    pub game_name: String,
    /// Its description.
    pub description: Option<String>,
    /// Its category.
    pub category: Option<String>,
    /// `exact`: every file of the game (its CUE aside) is a track or file
    /// of the artifact, and the artifact has none left over. `partial`:
    /// some are, as when audio tracks were read without offset correction.
    pub kind: String,
    /// How many of the game's files matched.
    pub files_matched: i32,
    /// How many files the game has, its CUE aside.
    pub files_total: i32,
    /// Each of them.
    pub files: Vec<DatMatchFileView>,
}

impl From<MatchRecord> for DatMatchView {
    fn from(record: MatchRecord) -> Self {
        Self {
            dat_id: record.dat_id.to_string(),
            dat_name: record.dat_name,
            dat_version: record.dat_version,
            game_name: record.game_name,
            description: record.description,
            category: record.category,
            kind: record.kind,
            files_matched: record.roms_matched,
            files_total: record.roms_total,
            files: record
                .roms
                .into_iter()
                .map(|rom| DatMatchFileView {
                    name: rom.name,
                    size: rom.size,
                    matched_by: rom.matched_by,
                })
                .collect(),
        }
    }
}

/// What identifies an artifact.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IdentificationView {
    /// `pending` (waiting for the background runner), `done` or `failed`.
    pub state: String,
    /// Why computing them last failed.
    pub error: Option<String>,
    /// When they were computed.
    pub hashed_at: Option<String>,
    /// Each component's hashes.
    pub components: Vec<ComponentHashesView>,
    /// Each CD track's hashes, for an artifact with a track layout.
    pub tracks: Vec<TrackHashesView>,
    /// The games of loaded DATs it matches, exact ones first.
    pub matches: Vec<DatMatchView>,
}

/// The identification view of `id`, or `NOT_FOUND`.
async fn view(state: &ApiState, id: ArtifactId) -> Result<IdentificationView, Problem> {
    let found = artifact_hashes(state.database().pool(), *id.as_uuid())
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read an artifact's hashes");
            unavailable("the hashes could not be read")
        })?
        .ok_or_else(|| Problem::not_found("artifact", &id.to_string()))?;
    let matches = matches_for(state.database().pool(), *id.as_uuid())
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read an artifact's DAT matches");
            unavailable("the matches could not be read")
        })?;
    Ok(IdentificationView {
        state: found.state,
        error: found.error,
        hashed_at: found.hashed_at.map(rfc3339),
        components: found
            .components
            .into_iter()
            .map(|c| ComponentHashesView {
                logical_path: c.logical_path,
                hashes: HashesView {
                    length_bytes: c.length_bytes,
                    crc32: c.crc32,
                    md5: c.md5,
                    sha1: c.sha1,
                },
            })
            .collect(),
        tracks: found
            .tracks
            .into_iter()
            .map(|t| TrackHashesView {
                number: t.number,
                hashes: HashesView {
                    length_bytes: t.length_bytes,
                    crc32: Some(t.crc32),
                    md5: Some(t.md5),
                    sha1: Some(t.sha1),
                },
            })
            .collect(),
        matches: matches.into_iter().map(DatMatchView::from).collect(),
    })
}

/// An artifact's preservation hashes and what they identify.
///
/// # Errors
///
/// `NOT_FOUND` for no such artifact.
#[utoipa::path(
    get,
    path = "/api/v1/artifacts/{artifact_id}/identification",
    tag = "library",
    description = "The CRC32, MD5 and SHA-1 of the artifact's components and CD tracks, as \
                   preservation databases list them, computed in the background after import, \
                   and the games of loaded DATs they match.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    responses(
        (status = 200, description = "Its identification", body = IdentificationView),
        (status = 404, description = "No such artifact", body = Problem),
    ),
)]
pub async fn get_identification(
    State(state): State<ApiState>,
    Path(artifact_id): Path<String>,
) -> Result<Json<IdentificationView>, Problem> {
    let id = parse_artifact(&artifact_id)?;
    Ok(Json(view(&state, id).await?))
}

/// Compute an artifact's preservation hashes again.
///
/// # Errors
///
/// `NOT_FOUND` for no such artifact.
#[utoipa::path(
    post,
    path = "/api/v1/artifacts/{artifact_id}/identification/rehash",
    tag = "library",
    description = "Compute the artifact's preservation hashes again, in the background: after \
                   a failure, or after its stored bytes were restored from a backup.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    responses(
        (status = 202, description = "Queued", body = IdentificationView),
        (status = 404, description = "No such artifact", body = Problem),
    ),
)]
pub async fn rehash(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(artifact_id): Path<String>,
) -> Result<(StatusCode, Json<IdentificationView>), Problem> {
    let id = parse_artifact(&artifact_id)?;
    let found = request_hashes(state.database().pool(), *id.as_uuid(), &user.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not ask for hashes");
            unavailable("the hashes could not be asked for")
        })?;
    if !found {
        return Err(Problem::not_found("artifact", &id.to_string()));
    }
    tracing::info!(artifact_id = %id, by = %user.username, "preservation hashes asked for again");
    if let Some(identification) = state.identification() {
        identification.nudge();
    }
    Ok((StatusCode::ACCEPTED, Json(view(&state, id).await?)))
}

/// The identification routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/artifacts/{artifact_id}/identification",
            get(get_identification),
        )
        .route(
            "/artifacts/{artifact_id}/identification/rehash",
            post(rehash),
        )
}
