// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The worker's side of the protocol, over HTTP.
//!
//! Everything a burn worker sends to the server and everything it stages from
//! it. The types here mirror the wire format rather than sharing structs with
//! the server: the dependency direction puts HTTP concerns above this crate,
//! and a worker is a client that could equally be somebody else's. The
//! published OpenAPI document is the contract, and a test in the server crate
//! asserts the two agree.
//!
//! Three rules shape the code:
//!
//! - **Every request has a timeout.** A worker that hangs on a socket is a
//!   drive nobody can use and a lease quietly expiring.
//! - **Every download is verified.** Staged bytes are hashed as they land and
//!   compared against the manifest, because a burn starts only after hash
//!   verification and the disc cannot be un-written afterwards.
//! - **Credentials never reach a log.** They are held in one place and only
//!   ever set on a header.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tangible_domain::manifest::ArtifactManifest;
use tangible_domain::{ArtifactId, BurnAttemptId, ComponentId, Sha256Digest};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::worker::{RecoveryRecord, WorkerEvent, WorkerStage};

/// How long any single protocol request may take.
///
/// Generous by API standards and deliberately so: a heartbeat racing a slow
/// reverse proxy should not cost a worker its lease. Downloads are bounded
/// separately, since a Blu-ray image legitimately takes far longer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a component download may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// Protocol version this client speaks.
pub const PROTOCOL_VERSION: &str = "1alpha1";

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server could not be reached, or the response was not usable.
    #[error("could not reach the server")]
    Transport(#[source] reqwest::Error),

    /// The server refused, and said why.
    ///
    /// Carries the stable code rather than only the status, because the
    /// worker branches on it: a revoked credential and a busy drive are both
    /// refusals and mean entirely different things.
    #[error("the server refused with {code} ({status})")]
    Refused {
        /// HTTP status.
        status: u16,
        /// Stable machine-readable code from the problem document.
        code: String,
        /// Human-readable detail.
        detail: String,
    },

    /// A download did not match the digest it was supposed to.
    ///
    /// Fatal for the attempt. The whole point of staging is that what is
    /// about to be written is what the library holds.
    #[error("{what} did not match its expected digest")]
    DigestMismatch {
        /// What was being fetched.
        what: String,
        /// The digest the server named.
        expected: String,
        /// The digest the bytes produced.
        observed: String,
    },

    /// A download was longer than the manifest said it would be.
    #[error("{what} was longer than its declared length")]
    TooLong {
        /// What was being fetched.
        what: String,
    },

    /// A manifest could not be parsed or failed its own validation.
    #[error("the manifest is not usable")]
    Manifest(#[source] tangible_domain::manifest::ManifestParseError),

    /// A staged file could not be written.
    #[error("{operation} failed for {path}")]
    Io {
        /// What was attempted.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
}

impl ClientError {
    /// Whether the credential this client holds is no longer usable.
    ///
    /// The one refusal a worker cannot retry its way out of: it must stop
    /// taking work and wait for an operator.
    #[must_use]
    pub fn is_unauthenticated(&self) -> bool {
        matches!(self, Self::Refused { status: 401, .. })
    }
}

/// A problem document, as the server sends it.
#[derive(Debug, Deserialize)]
struct ProblemBody {
    code: String,
    detail: String,
}

/// What a newly enrolled worker is given.
#[derive(Debug, Clone, Deserialize)]
pub struct Enrolled {
    /// Its identity.
    pub worker_id: String,
    /// The protocol version the server selected.
    pub protocol_version: String,
    /// The credential, returned exactly once and never recoverable.
    pub credential: String,
    /// How often to report in.
    pub heartbeat_interval_seconds: i64,
    /// How long a lease lasts.
    pub lease_duration_seconds: i64,
}

/// What the server says on a heartbeat.
#[derive(Debug, Clone, Deserialize)]
pub struct Heartbeat {
    /// Whether the worker should stop taking new work.
    pub drain: bool,
}

/// Where a worker fetches what it is to write.
#[derive(Debug, Clone, Deserialize)]
pub struct LeasedArtifact {
    /// The artifact.
    pub artifact_id: String,
    /// Path to fetch the manifest from.
    pub manifest_url: String,
    /// Digest of the manifest as published.
    #[serde(default)]
    pub manifest_sha256: Option<String>,
}

/// Work the server has leased to this worker.
#[derive(Debug, Clone, Deserialize)]
pub struct Leased {
    /// The attempt created for the claim.
    pub attempt_id: String,
    /// The job.
    pub burn_job_id: String,
    /// Which attempt this is.
    pub attempt_number: i32,
    /// What to write, and where to fetch it.
    pub artifact: LeasedArtifact,
    /// The lease token, presented on subsequent requests for this attempt.
    pub lease_token: String,
    /// When the lease lapses.
    pub lease_expires_at: String,
    /// Verification steps the job requires.
    pub verification_policy: Vec<String>,
    /// What to do with the disc when the attempt ends.
    pub eject_policy: String,
    /// Media profile the operator asked for, if any.
    #[serde(default)]
    pub requested_media_profile: Option<String>,
}

/// The engine's account of a write, as the protocol carries it.
#[derive(Debug, Clone, Serialize)]
pub struct WriteReportBody {
    /// `success`, `failed`, or `not_attempted`.
    pub state: String,
    /// Which engine wrote.
    pub engine: String,
    /// That engine's version.
    pub engine_version: String,
    /// When writing began.
    pub started_at: String,
    /// When it ended.
    pub completed_at: String,
}

/// The read-back comparison, as the protocol carries it.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationReportBody {
    /// Which verification ran.
    pub policy: String,
    /// `match`, `mismatch`, or `skipped`.
    pub state: String,
    /// How much was read back.
    pub bytes_read: i64,
    /// The digest the artifact should have.
    pub expected_sha256: String,
    /// The digest actually read from the disc.
    pub observed_sha256: String,
    /// What was checked on each track, for a disc described as tracks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<crate::plan::TrackVerification>,
}

/// What was in the drive.
#[derive(Debug, Clone, Serialize)]
pub struct PhysicalMediumBody {
    /// Media profile.
    pub profile: String,
    /// Manufacturer identifier, when the drive reported one.
    pub manufacturer_id: Option<String>,
    /// Media serial, when the drive reported one.
    pub serial: Option<String>,
}

/// Why an attempt did not succeed.
#[derive(Debug, Clone, Serialize)]
pub struct FailureBody {
    /// Stable machine-readable code.
    pub code: String,
    /// Human-readable detail.
    pub detail: Option<String>,
}

/// An erasure this worker has taken.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ErasureClaim {
    /// The erasure.
    pub erasure_id: String,
    /// `quick` or `full`.
    pub mode: String,
}

/// What was in the drive when an erasure was decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErasureMediumBody {
    /// The media profile.
    pub profile: String,
    /// Whether it was blank.
    pub blank: bool,
    /// Whether it can be erased.
    pub rewritable: bool,
    /// Sessions on it.
    pub sessions: u32,
}

/// How an erasure ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErasureCompletion {
    /// `erased`, `already_blank`, `refused` or `failed`.
    pub outcome: String,
    /// What was in the drive.
    pub medium: Option<ErasureMediumBody>,
    /// Why it did not erase.
    pub error_code: Option<String>,
    /// More about that.
    pub error_detail: Option<String>,
    /// How long it took, in seconds.
    pub duration_seconds: Option<u64>,
}

/// Everything a worker reports when an attempt ends.
#[derive(Debug, Clone, Serialize)]
pub struct Completion {
    /// The lease token from the claim.
    pub lease_token: String,
    /// The last event sequence the worker emitted.
    pub last_sequence: i64,
    /// The engine's account of the write.
    pub write_report: WriteReportBody,
    /// The read-back comparison, when one ran.
    pub verification_report: Option<VerificationReportBody>,
    /// What was in the drive.
    pub physical_medium: PhysicalMediumBody,
    /// Why it failed, when it did.
    pub failure: Option<FailureBody>,
}

/// What the server acknowledges when an attempt ends.
#[derive(Debug, Clone, Deserialize)]
pub struct CompletionAck {
    /// The disc recorded, when the attempt consumed media.
    #[serde(default)]
    pub physical_copy_id: Option<String>,
    /// Whether the drive should eject.
    pub eject: bool,
}

/// What a recovering worker is told to do.
#[derive(Debug, Clone, Deserialize)]
pub struct RecoveryOutcome {
    /// One of the protocol's recovery directives.
    pub directive: String,
    /// Whether local state may be dropped.
    pub discard_local_state: bool,
    /// Whether the worker may take new work.
    pub may_accept_new_work: bool,
    /// Always false. Restarting a write is never a recovery action.
    pub may_write: bool,
}

/// The drive identifier the server assigned.
#[derive(Debug, Clone, Deserialize)]
struct CapabilityAck {
    drive_id: String,
}

/// The renewed expiry.
#[derive(Debug, Deserialize)]
struct Renewed {
    lease_expires_at: String,
}

#[derive(Debug, Deserialize)]
struct EventAck {
    accepted_through_sequence: i64,
}

/// What a worker reports about itself and its drive.
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    /// The worker's software version.
    pub software_version: String,
    /// Engines it can run.
    pub engines: Vec<EngineDescription>,
    /// The drive it operates.
    pub drive: DriveDescription,
    /// Free bytes in its staging cache.
    pub cache_free_bytes: Option<i64>,
}

/// One engine a worker can run.
#[derive(Debug, Clone, Serialize)]
pub struct EngineDescription {
    /// Engine name.
    pub name: String,
    /// Its version.
    pub version: String,
}

/// What a worker says about its drive.
#[derive(Debug, Clone, Serialize)]
pub struct DriveDescription {
    /// Worker-local stable alias.
    pub device_alias: String,
    /// Human-meaningful name.
    pub configured_name: String,
    /// Reported vendor.
    pub vendor: Option<String>,
    /// Reported model.
    pub model: Option<String>,
    /// Reported firmware revision.
    pub firmware: Option<String>,
    /// SHA-256 of the drive serial, hashed here so the raw serial never
    /// leaves this machine.
    pub serial_hash: Option<String>,
    /// Current drive status.
    pub status: String,
    /// What the drive says it can do.
    pub capabilities: serde_json::Value,
}

/// Talks to one Tangible server as one worker.
#[derive(Debug, Clone)]
pub struct WorkerClient {
    http: reqwest::Client,
    base: String,
    credential: Option<String>,
}

impl WorkerClient {
    /// A client for the server at `base_url`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Transport`] if the HTTP client cannot be built.
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("tangible-burn-worker/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(ClientError::Transport)?;

        Ok(Self {
            http,
            // Trailing slashes are trimmed once here rather than guarded at
            // every call site.
            base: base_url.trim_end_matches('/').to_owned(),
            credential: None,
        })
    }

    /// Attach the credential this worker authenticates with.
    #[must_use]
    pub fn with_credential(mut self, credential: impl Into<String>) -> Self {
        self.credential = Some(credential.into());
        self
    }

    /// Whether a credential has been attached.
    #[must_use]
    pub fn is_enrolled(&self) -> bool {
        self.credential.is_some()
    }

    /// Build an absolute URL from a server-relative path.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn authorised(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.credential {
            // The one place the credential is used. It is never formatted
            // into a log line or an error.
            Some(credential) => request.bearer_auth(credential),
            None => request,
        }
    }

    /// Turn a non-success response into a refusal carrying its stable code.
    async fn refusal(response: reqwest::Response) -> ClientError {
        let status = response.status().as_u16();
        match response.json::<ProblemBody>().await {
            Ok(problem) => ClientError::Refused {
                status,
                code: problem.code,
                detail: problem.detail,
            },
            // Not every failure is a problem document: a reverse proxy can
            // answer 502 with HTML. The status is still worth branching on.
            Err(_) => ClientError::Refused {
                status,
                code: "UNKNOWN".to_owned(),
                detail: "the server returned a response this client could not read".to_owned(),
            },
        }
    }

    /// Exchange an enrollment token for a credential.
    ///
    /// # Errors
    ///
    /// [`ClientError::Refused`] if the token is not usable or the name is
    /// taken, or [`ClientError::Transport`] if the server cannot be reached.
    pub async fn enroll(
        &self,
        enrollment_token: &str,
        name: &str,
        software_version: &str,
    ) -> Result<Enrolled, ClientError> {
        let response = self
            .http
            .post(self.url("/api/v1/worker-enrollments/consume"))
            .json(&serde_json::json!({
                "enrollment_token": enrollment_token,
                "name": name,
                "protocol_versions": [PROTOCOL_VERSION],
                "software_version": software_version,
            }))
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        response.json().await.map_err(ClientError::Transport)
    }

    /// Report what this worker and its drive can do.
    ///
    /// Returns the drive identifier the server assigned, which is what claims
    /// are made against.
    ///
    /// # Errors
    ///
    /// [`ClientError::Refused`] if the report is rejected, or
    /// [`ClientError::Transport`] on a network failure.
    pub async fn report_capabilities(
        &self,
        worker_id: &str,
        capabilities: &Capabilities,
    ) -> Result<String, ClientError> {
        let response = self
            .authorised(
                self.http
                    .put(self.url(&format!("/api/v1/workers/{worker_id}/capabilities"))),
            )
            .json(capabilities)
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        let ack: CapabilityAck = response.json().await.map_err(ClientError::Transport)?;
        Ok(ack.drive_id)
    }

    /// Report that this worker is alive.
    ///
    /// # Errors
    ///
    /// [`ClientError::Refused`] if the credential is no longer valid, or
    /// [`ClientError::Transport`] on a network failure.
    pub async fn heartbeat(&self, worker_id: &str) -> Result<Heartbeat, ClientError> {
        let response = self
            .authorised(
                self.http
                    .post(self.url(&format!("/api/v1/workers/{worker_id}/heartbeat"))),
            )
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        response.json().await.map_err(ClientError::Transport)
    }

    /// Ask for work.
    ///
    /// `None` means there is nothing to do, which is the common answer and
    /// not an error.
    ///
    /// # Errors
    ///
    /// [`ClientError::Refused`] if the drive is already busy or the
    /// credential is not valid.
    pub async fn claim(
        &self,
        worker_id: &str,
        drive_id: &str,
        engine: &str,
        engine_version: &str,
    ) -> Result<Option<Leased>, ClientError> {
        let response = self
            .authorised(
                self.http
                    .post(self.url(&format!("/api/v1/workers/{worker_id}/claims"))),
            )
            .json(&serde_json::json!({
                "drive_id": drive_id,
                "engine": engine,
                "engine_version": engine_version,
            }))
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        response
            .json()
            .await
            .map(Some)
            .map_err(ClientError::Transport)
    }

    /// Take an erasure queued for this worker's drive.
    ///
    /// `None` means there is nothing to erase, the common answer.
    ///
    /// # Errors
    ///
    /// [`ClientError::Refused`] if the drive is not this worker's, or
    /// [`ClientError::Transport`] on a network failure.
    pub async fn claim_erasure(
        &self,
        worker_id: &str,
        drive_id: &str,
    ) -> Result<Option<ErasureClaim>, ClientError> {
        let response = self
            .authorised(
                self.http
                    .post(self.url(&format!("/api/v1/workers/{worker_id}/erasure-claims"))),
            )
            .json(&serde_json::json!({ "drive_id": drive_id }))
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        response
            .json()
            .await
            .map(Some)
            .map_err(ClientError::Transport)
    }

    /// Report how an erasure ended. Safe to repeat.
    ///
    /// # Errors
    ///
    /// [`ClientError::Refused`] if the server will not record it, or
    /// [`ClientError::Transport`] on a network failure.
    pub async fn complete_erasure(
        &self,
        erasure_id: &str,
        completion: &ErasureCompletion,
    ) -> Result<(), ClientError> {
        let response = self
            .authorised(
                self.http
                    .post(self.url(&format!("/api/v1/erasures/{erasure_id}/complete"))),
            )
            .json(completion)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        Ok(())
    }

    /// Extend a lease.
    ///
    /// `None` means the server would not renew. That does not stop a write in
    /// progress (nothing may); it means no further work should be claimed.
    ///
    /// # Errors
    ///
    /// [`ClientError::Transport`] on a network failure.
    pub async fn renew_lease(
        &self,
        attempt_id: BurnAttemptId,
        lease_token: &str,
    ) -> Result<Option<String>, ClientError> {
        let response = self
            .authorised(
                self.http
                    .post(self.url(&format!("/api/v1/burn-attempts/{attempt_id}/lease/renew"))),
            )
            .json(&serde_json::json!({ "lease_token": lease_token }))
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }

        let renewed: Renewed = response.json().await.map_err(ClientError::Transport)?;
        Ok(Some(renewed.lease_expires_at))
    }

    /// Submit a batch of events and learn what the server now holds.
    ///
    /// # Errors
    ///
    /// [`ClientError::Refused`] if the batch is rejected, or
    /// [`ClientError::Transport`] on a network failure.
    pub async fn submit_events(
        &self,
        attempt_id: BurnAttemptId,
        events: &[WorkerEvent],
    ) -> Result<u64, ClientError> {
        let response = self
            .authorised(
                self.http
                    .post(self.url(&format!("/api/v1/burn-attempts/{attempt_id}/events"))),
            )
            .json(&serde_json::json!({ "events": events }))
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        let ack: EventAck = response.json().await.map_err(ClientError::Transport)?;
        Ok(u64::try_from(ack.accepted_through_sequence).unwrap_or(0))
    }

    /// Report the end of an attempt.
    ///
    /// Safe to retry: the server answers a repeat with what the first report
    /// produced rather than recording a second disc.
    ///
    /// # Errors
    ///
    /// [`ClientError::Refused`] if no attempt holds that lease, or
    /// [`ClientError::Transport`] on a network failure.
    pub async fn complete(
        &self,
        attempt_id: BurnAttemptId,
        completion: &Completion,
    ) -> Result<CompletionAck, ClientError> {
        let response = self
            .authorised(
                self.http
                    .post(self.url(&format!("/api/v1/burn-attempts/{attempt_id}/complete"))),
            )
            .json(completion)
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        response.json().await.map_err(ClientError::Transport)
    }

    /// Reconcile local state after a restart.
    ///
    /// # Errors
    ///
    /// [`ClientError::Refused`] or [`ClientError::Transport`].
    pub async fn recover(
        &self,
        worker_id: &str,
        record: &RecoveryRecord,
        engine_running: bool,
    ) -> Result<RecoveryOutcome, ClientError> {
        let response = self
            .authorised(
                self.http
                    .post(self.url(&format!("/api/v1/workers/{worker_id}/recoveries"))),
            )
            .json(&serde_json::json!({
                "attempt_id": record.attempt_id.to_string(),
                "local_stage": stage_wire_name(record.stage),
                "last_event_sequence": record.last_event_sequence,
                "engine_process_state": if engine_running { "running" } else { "gone" },
            }))
            .send()
            .await
            .map_err(ClientError::Transport)?;

        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        response.json().await.map_err(ClientError::Transport)
    }

    /// Fetch and check a manifest.
    ///
    /// The digest is compared before the document is parsed, so a truncated
    /// or altered manifest is refused rather than half-understood.
    ///
    /// # Errors
    ///
    /// [`ClientError::DigestMismatch`] if the bytes do not match what the
    /// lease named, [`ClientError::Manifest`] if the document is not valid,
    /// or [`ClientError::Refused`] / [`ClientError::Transport`].
    pub async fn fetch_manifest(
        &self,
        manifest_url: &str,
        expected_sha256: Option<&str>,
    ) -> Result<ArtifactManifest, ClientError> {
        use sha2::Digest as _;

        let response = self
            .authorised(self.http.get(self.url(manifest_url)))
            .send()
            .await
            .map_err(ClientError::Transport)?;
        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        let bytes = response.bytes().await.map_err(ClientError::Transport)?;

        if let Some(expected) = expected_sha256 {
            let observed = hex::encode(sha2::Sha256::digest(&bytes));
            if observed != expected {
                return Err(ClientError::DigestMismatch {
                    what: "the manifest".to_owned(),
                    expected: expected.to_owned(),
                    observed,
                });
            }
        }

        let text = String::from_utf8_lossy(&bytes);
        ArtifactManifest::from_json(&text).map_err(ClientError::Manifest)
    }

    /// Download one component into the staging area, verifying as it lands.
    ///
    /// Resumable: a partial file left by an interrupted download is kept, its
    /// bytes are re-hashed, and the remainder is requested with a range. That
    /// is what makes a dropped connection during a twenty-five gigabyte
    /// staging cost minutes rather than the whole transfer.
    ///
    /// The digest is checked before the file is renamed into place, so a
    /// staged file that exists is a staged file that matched.
    ///
    /// # Errors
    ///
    /// [`ClientError::DigestMismatch`] if the bytes do not match the
    /// manifest, [`ClientError::TooLong`] if the server sends more than
    /// declared, or [`ClientError::Io`] if the file cannot be written.
    pub async fn download_component(
        &self,
        artifact_id: ArtifactId,
        component_id: ComponentId,
        expected: &Sha256Digest,
        expected_length: u64,
        destination: &Path,
    ) -> Result<(), ClientError> {
        use sha2::Digest as _;

        let partial = destination.with_extension("partial");
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| ClientError::Io {
                    operation: "creating the staging directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
        }

        let (mut hasher, mut have) = resume_point(&partial, expected_length).await?;

        if have < expected_length {
            let mut request = self
                .authorised(self.http.get(self.url(&format!(
                    "/api/v1/artifacts/{artifact_id}/components/{component_id}/content"
                ))))
                .timeout(DOWNLOAD_TIMEOUT);
            if have > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
            }

            let mut response = request.send().await.map_err(ClientError::Transport)?;
            if !response.status().is_success() {
                return Err(Self::refusal(response).await);
            }

            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&partial)
                .await
                .map_err(|source| ClientError::Io {
                    operation: "opening a partial download",
                    path: partial.clone(),
                    source,
                })?;

            while let Some(chunk) = response.chunk().await.map_err(ClientError::Transport)? {
                have += chunk.len() as u64;
                if have > expected_length {
                    return Err(ClientError::TooLong {
                        what: component_id.to_string(),
                    });
                }
                hasher.update(&chunk);
                file.write_all(&chunk)
                    .await
                    .map_err(|source| ClientError::Io {
                        operation: "writing a partial download",
                        path: partial.clone(),
                        source,
                    })?;
            }

            // Durable before the rename: a crash must leave either a complete
            // staged file or a partial one, never a named file with unwritten
            // tail blocks.
            file.flush().await.map_err(|source| ClientError::Io {
                operation: "flushing a partial download",
                path: partial.clone(),
                source,
            })?;
            file.sync_all().await.map_err(|source| ClientError::Io {
                operation: "syncing a partial download",
                path: partial.clone(),
                source,
            })?;
        }

        let observed = hex::encode(hasher.finalize());
        if observed != expected.to_hex() {
            // Keep nothing that failed: a partial with a wrong digest would
            // be resumed forever.
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(ClientError::DigestMismatch {
                what: component_id.to_string(),
                expected: expected.to_hex(),
                observed,
            });
        }

        tokio::fs::rename(&partial, destination)
            .await
            .map_err(|source| ClientError::Io {
                operation: "renaming a staged component",
                path: destination.to_path_buf(),
                source,
            })?;
        Ok(())
    }
}

/// Where a resumed download should carry on from, and the hash so far.
///
/// The partial file is re-read rather than trusted. A truncated or corrupted
/// resume would otherwise produce a digest that never matches, with nothing to
/// say why; re-hashing means the only thing carried forward is bytes that are
/// still on disk.
///
/// A partial longer than the manifest declares is discarded rather than
/// reasoned about: something wrote to it that should not have.
async fn resume_point(
    partial: &Path,
    expected_length: u64,
) -> Result<(sha2::Sha256, u64), ClientError> {
    use sha2::Digest as _;

    let mut hasher = sha2::Sha256::new();
    let mut have: u64 = 0;

    if let Ok(mut existing) = tokio::fs::File::open(partial).await {
        let mut buffer = vec![0_u8; 1 << 20];
        loop {
            let read = existing
                .read(&mut buffer)
                .await
                .map_err(|source| ClientError::Io {
                    operation: "reading a partial download",
                    path: partial.to_path_buf(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            have += read as u64;
        }
    }

    if have > expected_length {
        let _ = tokio::fs::remove_file(partial).await;
        return Ok((sha2::Sha256::new(), 0));
    }
    Ok((hasher, have))
}

/// The wire name of a stage.
fn stage_wire_name(stage: WorkerStage) -> &'static str {
    stage.as_str()
}

/// Format a timestamp the way the protocol expects.
#[must_use]
pub fn rfc3339(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// A worker's stored identity.
///
/// Written after enrollment and read on every start, because the credential is
/// returned exactly once: a worker that lost it would need an operator to
/// issue another enrollment token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerIdentity {
    /// The worker.
    pub worker_id: String,
    /// Its credential.
    pub credential: String,
    /// The drive the server assigned, when capabilities have been reported.
    #[serde(default)]
    pub drive_id: Option<String>,
}

impl WorkerIdentity {
    /// Read a stored identity, or `None` if this worker has not enrolled.
    ///
    /// # Errors
    ///
    /// [`ClientError::Io`] if the file exists but cannot be read.
    pub async fn load(path: &Path) -> Result<Option<Self>, ClientError> {
        match tokio::fs::read_to_string(path).await {
            Ok(text) => Ok(serde_json::from_str(&text).ok()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ClientError::Io {
                operation: "reading the worker identity",
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Write the identity, replacing any previous one.
    ///
    /// Written to a temporary file and renamed, so an interrupted write
    /// cannot leave a half-written credential that would strand the worker.
    ///
    /// # Errors
    ///
    /// [`ClientError::Io`] if the file cannot be written.
    pub async fn save(&self, path: &Path) -> Result<(), ClientError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| ClientError::Io {
                    operation: "creating the worker state directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        let temporary = path.with_extension("tmp");
        tokio::fs::write(&temporary, text)
            .await
            .map_err(|source| ClientError::Io {
                operation: "writing the worker identity",
                path: temporary.clone(),
                source,
            })?;
        tokio::fs::rename(&temporary, path)
            .await
            .map_err(|source| ClientError::Io {
                operation: "renaming the worker identity",
                path: path.to_path_buf(),
                source,
            })
    }
}

/// The identity of the worker that owns a state directory.
#[must_use]
pub fn identity_path(state_dir: &Path) -> PathBuf {
    state_dir.join("identity.json")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_base_url_keeps_one_slash_between_it_and_a_path() {
        let client = WorkerClient::new("http://server:8080/").expect("client");
        assert_eq!(client.url("/api/v1/x"), "http://server:8080/api/v1/x");

        let client = WorkerClient::new("http://server:8080").expect("client");
        assert_eq!(client.url("/api/v1/x"), "http://server:8080/api/v1/x");
    }

    #[test]
    fn a_client_without_a_credential_knows_it_is_not_enrolled() {
        let client = WorkerClient::new("http://server").expect("client");
        assert!(!client.is_enrolled());
        assert!(client.with_credential("tgw_live_x").is_enrolled());
    }

    #[test]
    fn an_unauthenticated_refusal_is_distinguishable() {
        // The one refusal a worker cannot retry its way out of.
        let revoked = ClientError::Refused {
            status: 401,
            code: "UNAUTHENTICATED".to_owned(),
            detail: String::new(),
        };
        assert!(revoked.is_unauthenticated());

        let busy = ClientError::Refused {
            status: 409,
            code: "CONFLICT".to_owned(),
            detail: String::new(),
        };
        assert!(!busy.is_unauthenticated());
    }

    #[test]
    fn stage_names_match_the_protocol() {
        assert_eq!(
            stage_wire_name(WorkerStage::WaitingForMedia),
            "waiting_for_media"
        );
        assert_eq!(stage_wire_name(WorkerStage::Writing), "writing");
    }

    #[tokio::test]
    async fn an_absent_identity_is_not_an_error() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = identity_path(dir.path());
        assert!(WorkerIdentity::load(&path).await.expect("load").is_none());
    }

    #[tokio::test]
    async fn an_identity_round_trips_through_the_state_directory() {
        // The credential is returned exactly once, so losing it costs an
        // operator an enrollment token.
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = identity_path(dir.path());
        let identity = WorkerIdentity {
            worker_id: "w".to_owned(),
            credential: "tgw_live_secret".to_owned(),
            drive_id: Some("d".to_owned()),
        };
        identity.save(&path).await.expect("save");

        let read = WorkerIdentity::load(&path)
            .await
            .expect("load")
            .expect("present");
        assert_eq!(read.worker_id, "w");
        assert_eq!(read.credential, "tgw_live_secret");
        assert_eq!(read.drive_id.as_deref(), Some("d"));
    }

    #[tokio::test]
    async fn an_unreadable_identity_is_treated_as_absent_rather_than_fatal() {
        // A corrupt file means enrolling again, which an operator can fix.
        // Refusing to start would leave a drive idle for a parse error.
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = identity_path(dir.path());
        tokio::fs::write(&path, "{ not json").await.expect("write");
        assert!(WorkerIdentity::load(&path).await.expect("load").is_none());
    }
}
