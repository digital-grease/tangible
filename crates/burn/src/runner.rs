// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The burn worker's run loop.
//!
//! Enroll, report what this machine can do, reconcile anything left over from
//! a previous life, then claim work and execute it: stage, preflight, write,
//! verify, report.
//!
//! The loop is written around one rule, and every apparent complication here
//! serves it: **a worker never starts a second write.** A disc is consumed the
//! moment writing begins, so the record that an attempt reached that point is
//! written to local disk *before* the laser runs, cleared only after the
//! server acknowledges the outcome, and consulted on every start. When the
//! worker cannot tell whether it wrote, it stops and asks for a human rather
//! than guessing.
//!
//! Everything else follows from ordinary care: bytes are verified against the
//! manifest before they are written, progress is reported by a task that keeps
//! running while the write blocks, and a lost lease never interrupts a write
//! in flight; it only stops the worker taking anything new.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tangible_domain::{ArtifactId, BurnAttemptId, BurnJobId, DriveId, Sha256Digest, WorkerId};
use time::OffsetDateTime;

use crate::client::{
    Capabilities, ClientError, Completion, DriveDescription, EngineDescription, FailureBody,
    Leased, PhysicalMediumBody, VerificationReportBody, WorkerClient, WorkerIdentity,
    WriteReportBody, identity_path, rfc3339,
};
use crate::device_lock::{DeviceLock, DeviceLockError};
use crate::engine::{BurnEngine, BurnEvent, EngineError, EventSink};
use crate::plan::{
    BurnPlan, DriveRef, MediumInfo, PlannedInput, PreflightFailure, VerifyReport, WriteMode,
    WriteReport,
};
use crate::worker::{EventBuffer, RecoveryRecord, RecoveryStore, WorkerStage, plan_for};

/// How a worker is configured.
#[derive(Debug, Clone)]
pub struct WorkerSettings {
    /// Base URL of the Tangible server.
    pub server_url: String,
    /// Name to enroll under, and the name an operator sees.
    pub worker_name: String,
    /// One-use enrollment token, needed only until this worker has a
    /// credential.
    pub enrollment_token: Option<String>,
    /// Where the credential, recovery record and staging cache live.
    ///
    /// Must survive a container restart, or every restart looks like a new
    /// worker and the record of what was in flight is lost.
    pub state_dir: PathBuf,
    /// Worker-local stable alias for the drive.
    pub device_alias: String,
    /// Human-meaningful drive name.
    pub configured_name: String,
    /// This worker's software version.
    pub software_version: String,
    /// How long to wait between polls when there is nothing to do.
    pub poll_interval: Duration,
    /// How long to wait for an operator to insert a usable disc.
    pub media_wait: Duration,
    /// How often to inspect the drive while waiting for media.
    pub media_poll_interval: Duration,
    /// How often to send buffered events and renew the lease during an
    /// attempt.
    pub report_interval: Duration,
}

impl WorkerSettings {
    /// Settings with the defaults a single-drive worker wants.
    #[must_use]
    pub fn new(
        server_url: impl Into<String>,
        worker_name: impl Into<String>,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            server_url: server_url.into(),
            worker_name: worker_name.into(),
            enrollment_token: None,
            state_dir,
            device_alias: "/dev/disc-block".to_owned(),
            configured_name: "drive".to_owned(),
            software_version: env!("CARGO_PKG_VERSION").to_owned(),
            poll_interval: Duration::from_secs(5),
            // Long enough that somebody can walk to the machine, short enough
            // that a forgotten job does not hold a drive overnight.
            media_wait: Duration::from_secs(30 * 60),
            media_poll_interval: Duration::from_secs(5),
            report_interval: Duration::from_secs(5),
        }
    }
}

/// Why the worker stopped.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The server refused something the worker cannot proceed without.
    #[error("the server refused")]
    Client(#[from] ClientError),

    /// No credential, and no enrollment token to get one with.
    #[error("this worker is not enrolled and no enrollment token was supplied")]
    NotEnrolled,

    /// Local state could not be read or written.
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

    /// The drive could not be claimed exclusively.
    ///
    /// Terminal, and deliberately so. Two workers sharing one drive is a
    /// deployment mistake, and the second one carrying on quietly is how two
    /// jobs come to be writing through one laser.
    #[error("the drive could not be claimed")]
    Drive(#[from] DeviceLockError),

    /// An attempt was in flight and its outcome cannot be established here.
    ///
    /// Terminal on purpose. The worker stops taking work and waits for a
    /// person, because the alternative is writing a second disc for a job
    /// that may already have one.
    #[error("a previous attempt needs a person before this worker can continue")]
    NeedsAttention,
}

/// Turns engine events into worker events.
///
/// Shared with the reporting task, so progress reaches the server while a
/// write is blocking this thread. The lock is held only long enough to push.
struct BufferSink {
    buffer: Arc<std::sync::Mutex<EventBuffer>>,
    fallback_stage: WorkerStage,
}

impl EventSink for BufferSink {
    fn emit(&self, event: BurnEvent) {
        let stage = event
            .stage
            .parse::<WorkerStage>()
            .unwrap_or(self.fallback_stage);
        // A poisoned lock drops the event rather than panicking: losing a
        // progress line is never worth aborting a burn over.
        if let Ok(mut buffer) = self.buffer.lock() {
            let _ = buffer.record(
                if event.progress.is_some() {
                    "progress"
                } else {
                    "stage_changed"
                },
                stage,
                // The code is the contract; the message is for a human and is
                // carried in the data.
                event.code,
                event.progress,
                serde_json::json!({ "message": event.message }),
                OffsetDateTime::now_utc(),
            );
        }
    }
}

/// Runs one worker against one server and one drive.
pub struct WorkerRuntime<E: BurnEngine> {
    settings: WorkerSettings,
    client: WorkerClient,
    engine: E,
    /// Held for as long as this worker runs, for engines that drive hardware.
    drive: Option<DeviceLock>,
}

impl<E: BurnEngine> WorkerRuntime<E> {
    /// Assemble a runtime.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Client`] if the HTTP client cannot be built.
    pub fn new(settings: WorkerSettings, engine: E) -> Result<Self, RunnerError> {
        let client = WorkerClient::new(&settings.server_url)?;
        Ok(Self {
            settings,
            client,
            engine,
            drive: None,
        })
    }

    fn recovery_store(&self) -> RecoveryStore {
        RecoveryStore::new(self.settings.state_dir.join("recovery.json"))
    }

    fn staging_root(&self) -> PathBuf {
        self.settings.state_dir.join("staging")
    }

    /// Load a stored credential, or enroll with the token supplied.
    ///
    /// # Errors
    ///
    /// [`RunnerError::NotEnrolled`] if there is neither, or
    /// [`RunnerError::Client`] if enrollment is refused.
    pub async fn identify(&mut self) -> Result<WorkerIdentity, RunnerError> {
        let path = identity_path(&self.settings.state_dir);
        if let Some(identity) = WorkerIdentity::load(&path).await? {
            tracing::info!(worker_id = %identity.worker_id, "using the stored credential");
            self.client = self
                .client
                .clone()
                .with_credential(identity.credential.clone());
            return Ok(identity);
        }

        let Some(token) = self.settings.enrollment_token.clone() else {
            return Err(RunnerError::NotEnrolled);
        };

        let enrolled = self
            .client
            .enroll(
                &token,
                &self.settings.worker_name,
                &self.settings.software_version,
            )
            .await?;
        tracing::info!(worker_id = %enrolled.worker_id, "enrolled");

        let identity = WorkerIdentity {
            worker_id: enrolled.worker_id,
            credential: enrolled.credential,
            drive_id: None,
        };
        // Stored before anything else is attempted: the credential is
        // returned exactly once, and losing it costs an operator another
        // enrollment token.
        identity.save(&path).await?;
        self.client = self
            .client
            .clone()
            .with_credential(identity.credential.clone());
        Ok(identity)
    }

    /// Report what this worker and its drive can do, and learn the drive's
    /// identifier.
    ///
    /// Called twice on purpose. The first report claims nothing about the
    /// drive, because the engine needs the server-assigned identifier before
    /// it can probe; the second carries what the probe found.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Client`] if the report is refused.
    pub async fn announce(&self, identity: &mut WorkerIdentity) -> Result<DriveId, RunnerError> {
        let describe = |capabilities: serde_json::Value, status: &str| Capabilities {
            software_version: self.settings.software_version.clone(),
            engines: vec![EngineDescription {
                name: self.engine.name().to_owned(),
                version: self.engine.version(),
            }],
            drive: DriveDescription {
                device_alias: self.settings.device_alias.clone(),
                configured_name: self.settings.configured_name.clone(),
                vendor: None,
                model: None,
                firmware: None,
                serial_hash: None,
                status: status.to_owned(),
                capabilities,
            },
            cache_free_bytes: None,
        };

        let drive_id = self
            .client
            .report_capabilities(
                &identity.worker_id,
                &describe(serde_json::json!({}), "unknown"),
            )
            .await?;
        let drive_id = drive_id
            .parse::<DriveId>()
            .map_err(|_| ClientError::Refused {
                status: 200,
                code: "MALFORMED_DRIVE_ID".to_owned(),
                detail: "the server returned a drive identifier this worker cannot parse"
                    .to_owned(),
            })?;

        let drive = self.drive_ref(identity, drive_id);
        match self.engine.probe_drive(&drive).await {
            Ok(capabilities) => {
                let json =
                    serde_json::to_value(&capabilities).unwrap_or_else(|_| serde_json::json!({}));
                let status = match self.engine.inspect_medium(&drive).await {
                    Ok(_) => "ready_with_media",
                    // Not an error: an empty drive is the normal resting
                    // state of a burner.
                    Err(_) => "ready_empty",
                };
                self.client
                    .report_capabilities(&identity.worker_id, &describe(json, status))
                    .await?;
            }
            Err(error) => {
                // Reported rather than fatal. A drive that cannot be probed
                // now may be usable later, and an operator watching the
                // workers page should see the state rather than a crash loop.
                tracing::warn!(error = ?error, "the drive could not be probed");
                self.client
                    .report_capabilities(
                        &identity.worker_id,
                        &describe(serde_json::json!({}), "error"),
                    )
                    .await?;
            }
        }

        identity.drive_id = Some(drive_id.to_string());
        identity
            .save(&identity_path(&self.settings.state_dir))
            .await?;
        Ok(drive_id)
    }

    fn drive_ref(&self, identity: &WorkerIdentity, drive_id: DriveId) -> DriveRef {
        DriveRef {
            worker_id: identity
                .worker_id
                .parse::<WorkerId>()
                .unwrap_or_else(|_| WorkerId::generate()),
            drive_id,
            device_alias: self.settings.device_alias.clone(),
        }
    }

    /// Reconcile anything a previous run left behind.
    ///
    /// # Errors
    ///
    /// [`RunnerError::NeedsAttention`] when the server's answer is that a
    /// person must look at the drive, or [`RunnerError::Client`] if the
    /// server cannot be reached.
    pub async fn reconcile(&self, identity: &WorkerIdentity) -> Result<(), RunnerError> {
        let store = self.recovery_store();
        let record = match store.read().await {
            Ok(None) => return Ok(()),
            Ok(Some(record)) => record,
            Err(error) => {
                // A record that cannot be read means an attempt was in flight
                // and its details are unknown. The pessimistic reading is the
                // safe one.
                tracing::error!(error = ?error, "the recovery record is unreadable");
                return Err(RunnerError::NeedsAttention);
            }
        };

        tracing::warn!(
            attempt_id = %record.attempt_id,
            stage = %record.stage,
            "reconciling an attempt left over from a previous run"
        );

        // The engine process cannot have survived this process, so it is
        // reported as gone. Saying otherwise would invite the server to
        // answer "reattach" to something that no longer exists.
        let outcome = self
            .client
            .recover(&identity.worker_id, &record, false)
            .await?;

        tracing::info!(directive = %outcome.directive, "recovery directive received");
        debug_assert!(!outcome.may_write, "no directive may ever permit a write");

        if outcome.discard_local_state {
            store.clear().await.map_err(|error| {
                tracing::error!(error = ?error, "could not clear the recovery record");
                RunnerError::NeedsAttention
            })?;
        }

        if outcome.may_accept_new_work {
            Ok(())
        } else {
            // Deliberately terminal. Continuing would mean claiming another
            // job while a disc this worker may have written is unaccounted
            // for.
            Err(RunnerError::NeedsAttention)
        }
    }

    /// Take the drive, for an engine that writes to one.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Drive`] if something else holds it.
    fn claim_drive(&mut self) -> Result<(), RunnerError> {
        if !self.engine.uses_hardware() {
            return Ok(());
        }

        // Released first, so a runtime that is run a second time reclaims its
        // own drive rather than colliding with itself.
        self.drive = None;
        self.drive = Some(DeviceLock::acquire(&self.settings.device_alias)?);
        tracing::info!(alias = %self.settings.device_alias, "drive claimed");
        Ok(())
    }

    /// Run until `shutdown` resolves.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Drive`] if this worker cannot have its drive,
    /// [`RunnerError::NeedsAttention`] if a previous attempt cannot be
    /// reconciled, [`RunnerError::NotEnrolled`] without a credential, or
    /// [`RunnerError::Client`] if the credential is revoked.
    pub async fn run<S>(&mut self, shutdown: S) -> Result<(), RunnerError>
    where
        S: Future<Output = ()> + Send,
    {
        // Before enrolling, and before announcing a drive. A worker that
        // cannot have this drive should not appear in the server's list of
        // workers that have one.
        self.claim_drive()?;

        let mut identity = self.identify().await?;
        let drive_id = self.announce(&mut identity).await?;
        self.reconcile(&identity).await?;

        let mut shutdown = Box::pin(shutdown);
        loop {
            let step = self.step(&identity, drive_id);
            tokio::select! {
                () = &mut shutdown => {
                    tracing::info!("shutting down after the current step");
                    return Ok(());
                }
                result = step => {
                    match result {
                        Ok(true) => {}
                        Ok(false) => {
                            // Nothing to do. Sleeping here rather than
                            // hammering the queue.
                            tokio::select! {
                                () = &mut shutdown => return Ok(()),
                                () = tokio::time::sleep(self.settings.poll_interval) => {}
                            }
                        }
                        Err(RunnerError::Client(error)) if error.is_unauthenticated() => {
                            // Revoked. Retrying cannot help and a worker that
                            // kept trying would fill an operator's logs with
                            // the same refusal.
                            tracing::error!("this worker's credential is no longer accepted");
                            return Err(RunnerError::Client(error));
                        }
                        Err(RunnerError::NeedsAttention) => return Err(RunnerError::NeedsAttention),
                        Err(error) => {
                            // Everything else is treated as weather: a server
                            // restart, a network blip. Log and try again.
                            tracing::warn!(error = ?error, "step failed; retrying");
                            tokio::select! {
                                () = &mut shutdown => return Ok(()),
                                () = tokio::time::sleep(self.settings.poll_interval) => {}
                            }
                        }
                    }
                }
            }
        }
    }

    /// One pass: report in, ask for work, do it if there is any.
    ///
    /// Returns whether work was performed.
    async fn step(
        &self,
        identity: &WorkerIdentity,
        drive_id: DriveId,
    ) -> Result<bool, RunnerError> {
        let heartbeat = self.client.heartbeat(&identity.worker_id).await?;
        if heartbeat.drain {
            tracing::info!("draining: taking no new work");
            return Ok(false);
        }

        let leased = self
            .client
            .claim(
                &identity.worker_id,
                &drive_id.to_string(),
                self.engine.name(),
                &self.engine.version(),
            )
            .await?;

        let Some(lease) = leased else {
            return Ok(false);
        };

        tracing::info!(
            attempt_id = %lease.attempt_id,
            burn_job_id = %lease.burn_job_id,
            "work leased"
        );
        self.execute(identity, drive_id, lease).await?;
        Ok(true)
    }
}

/// What an attempt produced, in the shape the protocol reports.
struct AttemptOutcome {
    write: WriteReportBody,
    verification: Option<VerificationReportBody>,
    medium: PhysicalMediumBody,
    failure: Option<FailureBody>,
}

impl<E: BurnEngine> WorkerRuntime<E> {
    /// Carry out one leased attempt, from staging to completion.
    async fn execute(
        &self,
        identity: &WorkerIdentity,
        drive_id: DriveId,
        lease: Leased,
    ) -> Result<(), RunnerError> {
        let attempt_id = lease
            .attempt_id
            .parse::<BurnAttemptId>()
            .map_err(|_| RunnerError::NeedsAttention)?;
        let buffer = Arc::new(std::sync::Mutex::new(EventBuffer::new(attempt_id)));
        let stop = Arc::new(AtomicBool::new(false));

        // Reporting runs alongside the work, because a write blocks for
        // minutes and an operator watching a progress bar should see it move.
        let reporter = tokio::spawn(report_loop(
            self.client.clone(),
            attempt_id,
            lease.lease_token.clone(),
            Arc::clone(&buffer),
            Arc::clone(&stop),
            self.settings.report_interval,
        ));

        let outcome = self
            .attempt(identity, drive_id, &lease, attempt_id, &buffer)
            .await;

        stop.store(true, Ordering::SeqCst);
        let _ = reporter.await;

        // One last flush, so the timeline the operator reads is complete
        // before the outcome lands on it.
        let pending = buffer
            .lock()
            .map_or_else(|_| Vec::new(), |buffer| buffer.batch(usize::MAX));
        if !pending.is_empty()
            && let Err(error) = self.client.submit_events(attempt_id, &pending).await
        {
            tracing::warn!(error = ?error, "could not submit the final events");
        }

        let last_sequence = buffer
            .lock()
            .map_or(0, |buffer| buffer.next_sequence().saturating_sub(1));

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // A failure this far in is still reported. An attempt the
                // server never hears about holds a drive until its lease
                // lapses and leaves an operator guessing.
                tracing::error!(error = ?error, "the attempt failed");
                AttemptOutcome {
                    write: write_report_body(
                        "not_attempted",
                        self.engine.name(),
                        &self.engine.version(),
                    ),
                    verification: None,
                    medium: PhysicalMediumBody {
                        profile: "unknown".to_owned(),
                        manufacturer_id: None,
                        serial: None,
                    },
                    failure: Some(FailureBody {
                        code: "WORKER_ERROR".to_owned(),
                        detail: Some(error.to_string()),
                    }),
                }
            }
        };

        let completion = Completion {
            lease_token: lease.lease_token.clone(),
            last_sequence: i64::try_from(last_sequence).unwrap_or(0),
            write_report: outcome.write,
            verification_report: outcome.verification,
            physical_medium: outcome.medium,
            failure: outcome.failure,
        };

        let ack = self.client.complete(attempt_id, &completion).await?;
        tracing::info!(
            attempt_id = %attempt_id,
            physical_copy_id = ?ack.physical_copy_id,
            "attempt completed"
        );

        // Only now: the record exists to answer "did this worker write a
        // disc the server does not know about", and until the server has
        // acknowledged, it does not know.
        if let Err(error) = self.recovery_store().clear().await {
            tracing::error!(error = ?error, "could not clear the recovery record");
        }

        if ack.eject
            && let Err(error) = self.engine.eject(&self.drive_ref(identity, drive_id)).await
        {
            tracing::warn!(error = ?error, "the drive would not eject");
        }

        Ok(())
    }

    /// Refuse a layout no disc could rescue, before asking for one.
    ///
    /// None of these answers change when media appears, so asking now saves an
    /// operator half an hour of waiting for a disc that was never going to be
    /// accepted. The warnings are emitted on the way past, because a track
    /// descriptor cannot carry subchannel data and audio cannot be verified
    /// byte for byte, and both are better known while the media is still a
    /// decision than explained afterwards.
    fn refuse_impossible_layout(
        &self,
        plan: &BurnPlan,
        buffer: &Arc<std::sync::Mutex<EventBuffer>>,
    ) -> Option<AttemptOutcome> {
        let mut refusals = plan.check_layout();
        if !self.engine.supports_mode(plan.mode) {
            refusals.push(PreflightFailure::WriteModeUnsupported {
                mode: plan.mode,
                engine: self.engine.name().to_owned(),
            });
        }

        if refusals.is_empty() {
            for warning in plan.layout_warnings() {
                warn(
                    buffer,
                    WorkerStage::Preflighting,
                    "LAYOUT_LIMITATION",
                    &warning,
                );
            }
            return None;
        }

        note(buffer, WorkerStage::Preflighting, "PREFLIGHT_FAILED");
        Some(AttemptOutcome {
            write: write_report_body("not_attempted", self.engine.name(), &self.engine.version()),
            verification: None,
            medium: PhysicalMediumBody {
                profile: "unknown".to_owned(),
                manufacturer_id: None,
                serial: None,
            },
            failure: Some(failure_of(&refusals)),
        })
    }

    /// Stage, preflight, write and verify. The part that touches media.
    async fn attempt(
        &self,
        identity: &WorkerIdentity,
        drive_id: DriveId,
        lease: &Leased,
        attempt_id: BurnAttemptId,
        buffer: &Arc<std::sync::Mutex<EventBuffer>>,
    ) -> Result<AttemptOutcome, RunnerError> {
        let drive = self.drive_ref(identity, drive_id);
        let artifact_id = lease
            .artifact
            .artifact_id
            .parse::<ArtifactId>()
            .map_err(|_| RunnerError::NeedsAttention)?;

        // --- stage ---------------------------------------------------------
        let record = RecoveryRecord {
            attempt_id,
            burn_job_id: lease
                .burn_job_id
                .parse::<BurnJobId>()
                .unwrap_or_else(|_| BurnJobId::generate()),
            stage: WorkerStage::Staging,
            drive_identity: self.settings.device_alias.clone(),
            artifact_id,
            manifest_sha256: Sha256Digest::from_bytes([0; 32]),
            last_event_sequence: 0,
            engine: self.engine.name().to_owned(),
            updated_at: OffsetDateTime::now_utc(),
        };
        self.write_record(&record).await?;

        let mut plan = self
            .stage(lease, attempt_id, artifact_id, &drive, buffer)
            .await?;

        // --- what cannot work whatever disc appears -------------------------
        if let Some(refused) = self.refuse_impossible_layout(&plan, buffer) {
            return Ok(refused);
        }

        // --- preflight, and waiting for a disc ------------------------------
        let preflight = self.wait_for_media(&mut plan, buffer).await?;
        let medium = preflight.medium.clone().unwrap_or_else(|| MediumInfo {
            profile: "unknown".to_owned(),
            ..MediumInfo::default()
        });
        let medium_body = PhysicalMediumBody {
            profile: medium.profile.clone(),
            manufacturer_id: medium.manufacturer_id.clone(),
            serial: None,
        };

        if !preflight.passed() {
            // Nothing was written, and saying so is the point: the completion
            // that follows records no disc.
            note(buffer, WorkerStage::Preflighting, "PREFLIGHT_FAILED");
            return Ok(AttemptOutcome {
                write: write_report_body(
                    "not_attempted",
                    self.engine.name(),
                    &self.engine.version(),
                ),
                verification: None,
                medium: medium_body,
                failure: Some(failure_of(&preflight.failures)),
            });
        }
        for warning in &preflight.warnings {
            tracing::info!(warning = %warning, "preflight warning");
        }

        // The last question preflight asks, and the one the engine cannot:
        // it is about this worker rather than about the medium. The drive was
        // claimed at startup, and an alias that now resolves elsewhere means
        // the thing about to be written is not the thing being held.
        if let Some(drive) = &self.drive
            && !drive.identity_intact()
        {
            tracing::error!(alias = %self.settings.device_alias, "the claimed drive changed");
            note(buffer, WorkerStage::Preflighting, "DRIVE_LOCK_LOST");
            return Ok(AttemptOutcome {
                write: write_report_body(
                    "not_attempted",
                    self.engine.name(),
                    &self.engine.version(),
                ),
                verification: None,
                medium: medium_body,
                failure: Some(FailureBody {
                    code: "DRIVE_LOCK_LOST".to_owned(),
                    detail: Some(format!(
                        "{} no longer names the drive this worker claimed",
                        drive.path().display()
                    )),
                }),
            });
        }

        self.write_and_verify(lease, &plan, &record, medium_body, buffer)
            .await
    }

    /// Write the disc and read it back. Everything past the point of no
    /// return.
    async fn write_and_verify(
        &self,
        lease: &Leased,
        plan: &BurnPlan,
        record: &RecoveryRecord,
        medium_body: PhysicalMediumBody,
        buffer: &Arc<std::sync::Mutex<EventBuffer>>,
    ) -> Result<AttemptOutcome, RunnerError> {
        // The irreversible transition. The record goes to disk first, and it
        // is what a restarted worker reads to know it must not write again.
        self.write_record(&RecoveryRecord {
            stage: WorkerStage::Writing,
            updated_at: OffsetDateTime::now_utc(),
            ..record.clone()
        })
        .await?;

        let started_at = OffsetDateTime::now_utc();
        let sink = BufferSink {
            buffer: Arc::clone(buffer),
            fallback_stage: WorkerStage::Writing,
        };
        let write = self.engine.write(plan, &sink).await;
        let completed_at = OffsetDateTime::now_utc();

        let write: WriteReport = match write {
            Ok(report) => report,
            Err(error) => {
                // Media was being consumed when this failed, so the disc is
                // real and probably ruined. Reported as a failed write, never
                // as one that did not happen.
                tracing::error!(error = ?error, "the write failed");
                return Ok(AttemptOutcome {
                    write: WriteReportBody {
                        state: "failed".to_owned(),
                        engine: self.engine.name().to_owned(),
                        engine_version: self.engine.version(),
                        started_at: rfc3339(started_at),
                        completed_at: rfc3339(completed_at),
                    },
                    verification: None,
                    medium: medium_body,
                    failure: Some(FailureBody {
                        code: engine_failure_code(&error).to_owned(),
                        detail: Some(error.to_string()),
                    }),
                });
            }
        };

        // --- verify --------------------------------------------------------
        let verification = if verification_reads_media_back(&lease.verification_policy) {
            self.write_record(&RecoveryRecord {
                stage: WorkerStage::Verifying,
                updated_at: OffsetDateTime::now_utc(),
                ..record.clone()
            })
            .await?;

            let sink = BufferSink {
                buffer: Arc::clone(buffer),
                fallback_stage: WorkerStage::Verifying,
            };
            match self.engine.verify(plan, &sink).await {
                Ok(report) => Some(verification_body(&lease.verification_policy, plan, &report)),
                Err(error) => {
                    // The disc exists and nothing read it back. Recorded as
                    // unverified rather than assumed good.
                    tracing::error!(error = ?error, "verification could not run");
                    None
                }
            }
        } else {
            None
        };

        self.write_record(&RecoveryRecord {
            stage: WorkerStage::Completing,
            updated_at: OffsetDateTime::now_utc(),
            ..record.clone()
        })
        .await?;
        note(buffer, WorkerStage::Completing, "ATTEMPT_COMPLETE");

        Ok(AttemptOutcome {
            write: WriteReportBody {
                state: if write.engine_reported_success {
                    "success".to_owned()
                } else {
                    "failed".to_owned()
                },
                engine: write.engine.clone(),
                engine_version: write.engine_version.clone(),
                started_at: rfc3339(started_at),
                completed_at: rfc3339(completed_at),
            },
            verification,
            medium: medium_body,
            failure: None,
        })
    }

    /// Download everything the burn needs and build the plan.
    ///
    /// Every component is verified against the manifest as it lands, which is
    /// what the invariant "a burn starts only after hash verification" means
    /// in practice: if these bytes are wrong, nothing is written.
    async fn stage(
        &self,
        lease: &Leased,
        attempt_id: BurnAttemptId,
        artifact_id: ArtifactId,
        drive: &DriveRef,
        buffer: &Arc<std::sync::Mutex<EventBuffer>>,
    ) -> Result<BurnPlan, RunnerError> {
        note(buffer, WorkerStage::Staging, "STAGING_STARTED");

        let manifest = self
            .client
            .fetch_manifest(
                &lease.artifact.manifest_url,
                lease.artifact.manifest_sha256.as_deref(),
            )
            .await?;

        let mut inputs = Vec::with_capacity(manifest.components.len());
        let mut input_of: std::collections::BTreeMap<tangible_domain::ComponentId, usize> =
            std::collections::BTreeMap::new();
        for component in &manifest.components {
            let destination = self
                .staging_root()
                .join(artifact_id.to_string())
                .join(component.logical_path.to_string());
            self.client
                .download_component(
                    artifact_id,
                    component.id,
                    &component.content.sha256,
                    component.length_bytes,
                    &destination,
                )
                .await?;
            input_of.insert(component.id, inputs.len());
            inputs.push(PlannedInput {
                staged_path: destination,
                sha256: component.content.sha256,
                length_bytes: component.length_bytes,
            });
        }
        note(buffer, WorkerStage::Staging, "STAGING_COMPLETE");

        let tracks = planned_tracks(&manifest.topology, &input_of);
        let total_bytes = inputs.iter().map(|input| input.length_bytes).sum();
        Ok(BurnPlan {
            attempt_id,
            drive: drive.clone(),
            inputs,
            // A disc described as tracks has to be written as a whole table of
            // contents in one pass. Track-at-once inserts its own two second
            // gaps and cannot honour an INDEX 00, so a mixed-mode disc written
            // that way is a different disc from the one described.
            mode: if tracks.is_empty() {
                WriteMode::DataDiscAtOnce
            } else {
                WriteMode::TocDiscAtOnce
            },
            tracks,
            // Empty when the job named no profile. Preflight fills it in
            // from the disc actually in the drive, because a plan that
            // accepts nothing would refuse every medium.
            accepted_profiles: lease
                .requested_media_profile
                .clone()
                .map(|profile| vec![profile])
                .unwrap_or_default(),
            speed: None,
            finalize: true,
            eject_on_success: lease.eject_policy == "eject_on_success",
            total_bytes,
        })
    }

    /// Run preflight, waiting for an operator to insert a usable disc.
    ///
    /// Preflight is re-run against whatever was actually inserted rather than
    /// trusting the first look, because the disc that appears may not be the
    /// disc that was asked for.
    async fn wait_for_media(
        &self,
        plan: &mut BurnPlan,
        buffer: &Arc<std::sync::Mutex<EventBuffer>>,
    ) -> Result<crate::plan::PreflightReport, RunnerError> {
        let deadline = tokio::time::Instant::now() + self.settings.media_wait;
        let mut announced = false;
        let job_named_a_profile = !plan.accepted_profiles.is_empty();

        loop {
            if !job_named_a_profile {
                // The operator asked for "whatever fits", so the plan is told
                // what is actually in the drive before preflight judges it.
                // Refreshed every round because the disc can change between
                // rounds; that is the whole point of waiting.
                match self.engine.inspect_medium(&plan.drive).await {
                    Ok(medium) => plan.accepted_profiles = vec![medium.profile],
                    Err(_) => plan.accepted_profiles.clear(),
                }
            }

            note(buffer, WorkerStage::Preflighting, "PREFLIGHT_STARTED");
            let report = self.engine.preflight(&*plan).await.map_err(|error| {
                tracing::error!(error = ?error, "preflight could not run");
                RunnerError::NeedsAttention
            })?;

            if report.passed() || !waiting_on_media(&report.failures) {
                return Ok(report);
            }

            if !announced {
                // The one thing on the operator's screen that needs a person
                // to get up, so it is said once and clearly rather than on
                // every poll.
                note(buffer, WorkerStage::WaitingForMedia, "MEDIA_REQUIRED");
                announced = true;
            }

            if tokio::time::Instant::now() >= deadline {
                note(buffer, WorkerStage::WaitingForMedia, "MEDIA_WAIT_EXPIRED");
                return Ok(report);
            }
            tokio::time::sleep(self.settings.media_poll_interval).await;
        }
    }

    async fn write_record(&self, record: &RecoveryRecord) -> Result<(), RunnerError> {
        self.recovery_store().write(record).await.map_err(|error| {
            tracing::error!(error = ?error, "could not write the recovery record");
            // Without a durable record this worker could not tell, after a
            // crash, whether it had written. Refusing to continue is the
            // only safe answer.
            RunnerError::NeedsAttention
        })
    }
}

/// Record a stage event with no progress figure.
fn note(buffer: &Arc<std::sync::Mutex<EventBuffer>>, stage: WorkerStage, code: &str) {
    if let Ok(mut buffer) = buffer.lock() {
        let _ = buffer.record(
            "stage_changed",
            stage,
            code,
            None,
            serde_json::json!({}),
            OffsetDateTime::now_utc(),
        );
    }
}

/// Send buffered events and renew the lease until told to stop.
///
/// Separate from the work so a write that blocks for twenty minutes still
/// reports. A failure here never interrupts the write: the events stay
/// buffered and go out on the next pass or at completion.
async fn report_loop(
    client: WorkerClient,
    attempt_id: BurnAttemptId,
    lease_token: String,
    buffer: Arc<std::sync::Mutex<EventBuffer>>,
    stop: Arc<AtomicBool>,
    interval: Duration,
) {
    while !stop.load(Ordering::SeqCst) {
        tokio::time::sleep(interval).await;

        let pending = buffer
            .lock()
            .map_or_else(|_| Vec::new(), |buffer| buffer.batch(256));
        if !pending.is_empty() {
            match client.submit_events(attempt_id, &pending).await {
                Ok(through) => {
                    if let Ok(mut buffer) = buffer.lock() {
                        buffer.acknowledge(through);
                    }
                }
                // Kept, not dropped: the buffer exists so a disconnected
                // worker can resend everything when the server returns.
                Err(error) => tracing::warn!(error = ?error, "could not submit events"),
            }
        }

        match client.renew_lease(attempt_id, &lease_token).await {
            Ok(Some(_)) => {}
            // A refused renewal does not stop a write. It means this worker
            // must not claim anything further, which the loop enforces by
            // finishing what it holds and asking again.
            Ok(None) => tracing::warn!("the lease was not renewed"),
            Err(error) => tracing::warn!(error = ?error, "lease renewal failed"),
        }
    }
}

fn write_report_body(state: &str, engine: &str, version: &str) -> WriteReportBody {
    let now = rfc3339(OffsetDateTime::now_utc());
    WriteReportBody {
        state: state.to_owned(),
        engine: engine.to_owned(),
        engine_version: version.to_owned(),
        started_at: now.clone(),
        completed_at: now,
    }
}

/// Whether the policy asks for the disc to be read back.
fn verification_reads_media_back(policy: &[String]) -> bool {
    policy.iter().any(|step| {
        step.parse::<tangible_domain::VerificationStep>()
            .is_ok_and(|step| step.reads_media_back())
    })
}

/// Turn an engine's verify report into what the protocol carries.
fn verification_body(
    policy: &[String],
    plan: &BurnPlan,
    report: &VerifyReport,
) -> VerificationReportBody {
    let expected = plan
        .inputs
        .first()
        .map(|input| input.sha256.to_hex())
        .unwrap_or_default();
    VerificationReportBody {
        policy: policy
            .iter()
            .find(|step| {
                step.parse::<tangible_domain::VerificationStep>()
                    .is_ok_and(|step| step.reads_media_back())
            })
            .cloned()
            .unwrap_or_else(|| "full_sector_readback".to_owned()),
        state: if report.matched { "match" } else { "mismatch" }.to_owned(),
        bytes_read: i64::try_from(report.bytes_compared).unwrap_or(i64::MAX),
        // A mismatch reports a digest that is deliberately not the expected
        // one: the engine compares as it reads and does not produce a whole
        // digest of a disc it already knows differs.
        observed_sha256: if report.matched {
            expected.clone()
        } else {
            String::new()
        },
        expected_sha256: expected,
    }
}

/// Whether preflight is only waiting for someone to put a disc in.
fn waiting_on_media(failures: &[PreflightFailure]) -> bool {
    !failures.is_empty()
        && failures.iter().all(|failure| {
            matches!(
                failure,
                PreflightFailure::NoMedium
                    | PreflightFailure::MediumNotWritable { .. }
                    | PreflightFailure::MediumNotBlank { .. }
                    | PreflightFailure::ProfileNotAccepted { .. }
                    | PreflightFailure::InsufficientCapacity { .. }
            )
        })
}

/// Turn a manifest's topology into the tracks a plan carries.
///
/// A topology naming a component the manifest does not list is refused by
/// manifest validation before this runs, so the lookup cannot normally miss.
/// When it does, the track is kept with an input index that cannot resolve
/// rather than dropped: a plan quietly missing a track would write a disc
/// missing a track, and preflight is where that should be said out loud.
fn planned_tracks(
    topology: &tangible_domain::manifest::Topology,
    input_of: &std::collections::BTreeMap<tangible_domain::ComponentId, usize>,
) -> Vec<crate::plan::PlannedTrack> {
    let tangible_domain::manifest::Topology::CdTracks { tracks, .. } = topology else {
        return Vec::new();
    };

    tracks
        .iter()
        .map(|track| crate::plan::PlannedTrack {
            number: track.number,
            session: track.session,
            mode: track.mode.clone(),
            input: input_of
                .get(&track.component_id)
                .copied()
                .unwrap_or(usize::MAX),
            file_offset_bytes: track.file_offset_bytes,
            start_lba: track.start_lba,
            sector_count: track.sector_count,
            pregap_sectors: track.pregap_sectors,
            indexes: track
                .indexes
                .iter()
                .map(|index| crate::plan::PlannedIndex {
                    number: index.number,
                    relative_lba: index.relative_lba,
                })
                .collect(),
        })
        .collect()
}

/// Record a warning an operator should see.
///
/// Distinct from [`note`], which records that a stage happened. A warning
/// carries prose, and it goes into the event stream rather than only into a
/// log file because the person who needs it is watching a progress page and
/// deciding whether to let a disc be consumed.
fn warn(
    buffer: &Arc<std::sync::Mutex<EventBuffer>>,
    stage: WorkerStage,
    code: &str,
    message: &str,
) {
    if let Ok(mut buffer) = buffer.lock() {
        let _ = buffer.record(
            "warning",
            stage,
            code,
            None,
            serde_json::json!({ "message": message }),
            OffsetDateTime::now_utc(),
        );
    }
}

/// A stable code and a readable reason for the first blocking failure.
fn failure_of(failures: &[PreflightFailure]) -> FailureBody {
    let code = match failures.first() {
        Some(PreflightFailure::NoMedium) => "PREFLIGHT_NO_MEDIUM",
        Some(PreflightFailure::MediumNotWritable { .. }) => "PREFLIGHT_MEDIUM_NOT_WRITABLE",
        Some(PreflightFailure::MediumNotBlank { .. }) => "PREFLIGHT_MEDIUM_NOT_BLANK",
        Some(PreflightFailure::ProfileNotAccepted { .. }) => "PREFLIGHT_PROFILE_NOT_ACCEPTED",
        Some(PreflightFailure::InsufficientCapacity { .. }) => "PREFLIGHT_INSUFFICIENT_CAPACITY",
        Some(PreflightFailure::DriveCannotWriteProfile { .. }) => "PREFLIGHT_DRIVE_CANNOT_WRITE",
        Some(PreflightFailure::InputDigestMismatch { .. }) => "PREFLIGHT_INPUT_DIGEST_MISMATCH",
        Some(PreflightFailure::InputMissing { .. }) => "PREFLIGHT_INPUT_MISSING",
        Some(PreflightFailure::UnknownTrackMode { .. }) => "PREFLIGHT_UNKNOWN_TRACK_MODE",
        Some(PreflightFailure::TrackOutsideInput { .. }) => "PREFLIGHT_TRACK_OUTSIDE_INPUT",
        Some(PreflightFailure::TrackInputMissing { .. }) => "PREFLIGHT_TRACK_INPUT_MISSING",
        Some(PreflightFailure::TooManyTracks { .. }) => "PREFLIGHT_TOO_MANY_TRACKS",
        Some(PreflightFailure::MultisessionUnsupported { .. }) => "PREFLIGHT_MULTISESSION",
        Some(PreflightFailure::ExceedsCdCapacity { .. }) => "PREFLIGHT_EXCEEDS_CD_CAPACITY",
        Some(PreflightFailure::WriteModeUnsupported { .. }) => "PREFLIGHT_WRITE_MODE_UNSUPPORTED",
        Some(PreflightFailure::TrackLayoutNeedsACd { .. }) => "PREFLIGHT_NEEDS_A_CD",
        None => "PREFLIGHT_FAILED",
    };
    FailureBody {
        code: code.to_owned(),
        detail: serde_json::to_string(failures).ok(),
    }
}

/// A stable code for an engine failure.
fn engine_failure_code(error: &EngineError) -> &'static str {
    match error {
        EngineError::DriveUnavailable { .. } => "ENGINE_DRIVE_UNAVAILABLE",
        EngineError::Cancelled => "ENGINE_CANCELLED",
        _ => "ENGINE_FAILED",
    }
}

/// The recovery plan a directive implies, re-exported for callers that want
/// to reason about a directive without going through the client.
#[must_use]
pub fn recovery_plan(directive: crate::worker::RecoveryDirective) -> crate::worker::RecoveryPlan {
    plan_for(directive)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_that_reads_the_disc_back_is_recognised() {
        assert!(verification_reads_media_back(&[
            "full_sector_readback".to_owned()
        ]));
        assert!(verification_reads_media_back(&[
            "filesystem_compare".to_owned()
        ]));
    }

    #[test]
    fn a_policy_that_only_trusts_the_tool_is_not_verification() {
        // A successful tool exit is not verified media, so nothing here may
        // report a read-back that never happened.
        assert!(!verification_reads_media_back(&["tool_verify".to_owned()]));
        assert!(!verification_reads_media_back(&["none".to_owned()]));
        assert!(!verification_reads_media_back(&[]));
    }

    #[test]
    fn an_unknown_verification_step_does_not_imply_a_read_back() {
        assert!(!verification_reads_media_back(&["vibes".to_owned()]));
    }

    #[test]
    fn waiting_for_media_covers_only_things_a_person_can_fix() {
        // These are all "put a different disc in". Anything else is a real
        // failure and waiting would only burn the clock.
        assert!(waiting_on_media(&[PreflightFailure::NoMedium]));
        assert!(waiting_on_media(&[PreflightFailure::MediumNotBlank {
            sessions: 1
        }]));
        assert!(waiting_on_media(&[
            PreflightFailure::InsufficientCapacity {
                required_bytes: 10,
                available_bytes: 5
            }
        ]));

        assert!(!waiting_on_media(&[PreflightFailure::InputMissing {
            path: "x".to_owned()
        }]));
        assert!(!waiting_on_media(&[
            PreflightFailure::InputDigestMismatch {
                path: "x".to_owned(),
                expected: "a".to_owned(),
                actual: "b".to_owned(),
            }
        ]));
        // A mixed set is not a media wait: one of them cannot be fixed by
        // swapping the disc.
        assert!(!waiting_on_media(&[
            PreflightFailure::NoMedium,
            PreflightFailure::InputMissing {
                path: "x".to_owned()
            }
        ]));
        assert!(!waiting_on_media(&[]));
    }

    #[test]
    fn every_preflight_failure_has_its_own_code() {
        // An operator reading "PREFLIGHT_FAILED" learns nothing. Each of
        // these has a different remedy.
        let failures = [
            PreflightFailure::NoMedium,
            PreflightFailure::MediumNotWritable {
                profile: "CD-ROM".to_owned(),
            },
            PreflightFailure::MediumNotBlank { sessions: 1 },
            PreflightFailure::ProfileNotAccepted {
                found: "CD-R".to_owned(),
                accepted: vec!["BD-R".to_owned()],
            },
            PreflightFailure::InsufficientCapacity {
                required_bytes: 10,
                available_bytes: 5,
            },
            PreflightFailure::DriveCannotWriteProfile {
                profile: "BD-R".to_owned(),
            },
            PreflightFailure::InputMissing {
                path: "x".to_owned(),
            },
        ];
        let mut codes = std::collections::BTreeSet::new();
        for failure in failures {
            let body = failure_of(&[failure]);
            assert!(codes.insert(body.code.clone()), "duplicate {}", body.code);
            assert!(body.detail.is_some(), "the reason must survive");
        }
    }

    #[test]
    fn a_failure_report_survives_an_empty_failure_list() {
        assert_eq!(failure_of(&[]).code, "PREFLIGHT_FAILED");
    }

    #[test]
    fn settings_default_to_verifying_before_giving_up_on_media() {
        let settings = WorkerSettings::new("http://server", "w", PathBuf::from("/tmp/x"));
        assert!(settings.media_wait > settings.media_poll_interval);
        assert!(settings.report_interval < settings.media_wait);
    }
}
