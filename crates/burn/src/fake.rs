// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A burn engine that touches no hardware.
//!
//! The default engine everywhere except a deliberately configured hardware
//! deployment, and the reason the burn path can be developed and tested at all
//! without destroying discs.
//!
//! It is a real implementation rather than a stub. The "medium" is a file, and
//! writing appends to it while verification reads it back and compares byte
//! for byte. That makes read-back verification genuinely exercised: a bug that
//! writes the wrong bytes fails here exactly as it would on a real drive.
//!
//! Failures are configurable so the paths that matter can be tested
//! deliberately: a write that dies partway, a disc that reads back wrong, a
//! cancellation mid-write. Those are the cases worth having covered before a
//! real drive is involved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::engine::{BurnEngine, BurnEvent, EngineError, EventSink};
use crate::plan::{
    BlankReport, BlankRequest, BurnPlan, DriveCapabilities, DriveRef, MediumInfo, PreflightFailure,
    PreflightReport, VerifyReport, WriteMode, WriteReport,
};

/// How the fake engine should misbehave.
///
/// Every field defaults to well-behaved, so a test opts into exactly the
/// failure it is exercising and nothing else.
// Independent toggles, each selecting one failure to simulate. Collapsing
// them into an enum would prevent combining, e.g. a small medium *and* a
// write that dies partway.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct FakeBehaviour {
    /// Fail the write after this many bytes.
    pub fail_write_after_bytes: Option<u64>,
    /// Corrupt the medium after writing, so verification finds a mismatch.
    ///
    /// Simulates a drive that reports success while producing a bad disc,
    /// which is precisely why write success and verification are separate.
    pub corrupt_after_write: bool,
    /// Report the drive as unavailable.
    pub drive_unavailable: bool,
    /// Present no medium.
    pub no_medium: bool,
    /// Present a medium that cannot be written.
    pub medium_read_only: bool,
    /// Present a medium of this many blocks. Defaults to a CD-R.
    pub medium_blocks: Option<u64>,
    /// Present a medium that already holds sessions.
    pub medium_sessions: u32,
    /// Media profile to report.
    pub medium_profile: Option<String>,
}

/// State shared with whoever wants to cancel.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// A token that has not been triggered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the operation in flight to stop.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// Blocks on a blank CD-R, the default simulated medium.
const DEFAULT_MEDIUM_BLOCKS: u64 = 360_000;

/// How many chunks a write is reported in.
///
/// Enough that progress is visibly incremental without flooding the event
/// stream on a large image.
const PROGRESS_CHUNKS: u64 = 10;

/// A burn engine backed by a file standing in for the disc.
#[derive(Debug, Clone)]
pub struct FakeEngine {
    /// Where simulated media live, one file per drive.
    media_root: PathBuf,
    behaviour: FakeBehaviour,
    cancel: CancelToken,
}

impl FakeEngine {
    /// An engine writing simulated media beneath `media_root`.
    #[must_use]
    pub fn new(media_root: impl Into<PathBuf>) -> Self {
        Self {
            media_root: media_root.into(),
            behaviour: FakeBehaviour::default(),
            cancel: CancelToken::new(),
        }
    }

    /// Configure how it should misbehave.
    #[must_use]
    pub fn with_behaviour(mut self, behaviour: FakeBehaviour) -> Self {
        self.behaviour = behaviour;
        self
    }

    /// Attach a cancellation token.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// The token this engine watches.
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Path of the file standing in for the disc in one drive.
    #[must_use]
    pub fn medium_path(&self, drive: &DriveRef) -> PathBuf {
        self.media_root.join(format!("{}.medium", drive.drive_id))
    }

    fn check_cancelled(&self) -> Result<(), EngineError> {
        if self.cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        Ok(())
    }

    fn simulated_medium(&self) -> MediumInfo {
        let blocks = self
            .behaviour
            .medium_blocks
            .unwrap_or(DEFAULT_MEDIUM_BLOCKS);
        MediumInfo {
            profile: self
                .behaviour
                .medium_profile
                .clone()
                .unwrap_or_else(|| "CD-R".to_owned()),
            blank: self.behaviour.medium_sessions == 0,
            writable: !self.behaviour.medium_read_only,
            rewritable: false,
            capacity_blocks: blocks,
            free_blocks: blocks,
            block_size: 2048,
            manufacturer_id: Some("SIMULATED".to_owned()),
            sessions: self.behaviour.medium_sessions,
            erasable: false,
            status_warnings: vec![],
        }
    }
}

#[async_trait]
impl BurnEngine for FakeEngine {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    /// Never. The disc is a file, and there is nothing to hold exclusively.
    fn uses_hardware(&self) -> bool {
        false
    }

    /// Every shape. The fake engine exists to exercise the paths around it,
    /// and an engine that refused work would exercise fewer of them.
    fn supports_mode(&self, _mode: WriteMode) -> bool {
        true
    }

    async fn probe_drive(&self, drive: &DriveRef) -> Result<DriveCapabilities, EngineError> {
        if self.behaviour.drive_unavailable {
            return Err(EngineError::DriveUnavailable {
                alias: drive.device_alias.clone(),
            });
        }
        let mut speeds = BTreeMap::new();
        speeds.insert("CD-R".to_owned(), "48x".to_owned());
        speeds.insert("DVD-R".to_owned(), "16x".to_owned());

        let mut evidence = BTreeMap::new();
        evidence.insert("fake".to_owned(), "simulated capabilities".to_owned());

        Ok(DriveCapabilities {
            read_profiles: vec!["CD-ROM".to_owned(), "CD-R".to_owned(), "DVD-ROM".to_owned()],
            write_profiles: vec!["CD-R".to_owned(), "CD-RW".to_owned(), "DVD-R".to_owned()],
            supports_test_write: true,
            supports_buffer_underrun_protection: true,
            supports_cd_text: true,
            supports_raw_dao: true,
            supports_subchannel_rw: false,
            max_write_speed_by_profile: speeds,
            engine_evidence: evidence,
        })
    }

    async fn inspect_medium(&self, drive: &DriveRef) -> Result<MediumInfo, EngineError> {
        if self.behaviour.drive_unavailable {
            return Err(EngineError::DriveUnavailable {
                alias: drive.device_alias.clone(),
            });
        }
        if self.behaviour.no_medium {
            return Err(EngineError::UnsuitableMedium {
                reason: "no medium present".to_owned(),
            });
        }
        Ok(self.simulated_medium())
    }

    async fn preflight(&self, plan: &BurnPlan) -> Result<PreflightReport, EngineError> {
        if self.behaviour.drive_unavailable {
            return Err(EngineError::DriveUnavailable {
                alias: plan.drive.device_alias.clone(),
            });
        }

        let mut failures = Vec::new();
        let mut warnings = Vec::new();

        if self.behaviour.no_medium {
            return Ok(PreflightReport {
                failures: vec![PreflightFailure::NoMedium],
                warnings,
                medium: None,
            });
        }

        let medium = self.simulated_medium();

        if !medium.writable {
            failures.push(PreflightFailure::MediumNotWritable {
                profile: medium.profile.clone(),
            });
        }
        if medium.sessions > 0 {
            failures.push(PreflightFailure::MediumNotBlank {
                sessions: medium.sessions,
            });
        }
        if !plan.accepted_profiles.contains(&medium.profile) {
            failures.push(PreflightFailure::ProfileNotAccepted {
                found: medium.profile.clone(),
                accepted: plan.accepted_profiles.clone(),
            });
        }
        // Sectors for a track layout, bytes for a block image. Asked of the
        // plan so the fake engine agrees with the real one about what fits.
        failures.extend(plan.capacity_failures(&medium));

        // Re-verify every input against its recorded digest. This is what
        // makes "hash verified before write" a fact rather than an intention:
        // bytes staged an hour ago may not be the bytes on disk now.
        for input in &plan.inputs {
            match hash_file(&input.staged_path).await {
                Err(_) => failures.push(PreflightFailure::InputMissing {
                    path: input.staged_path.display().to_string(),
                }),
                Ok(actual) if actual != input.sha256 => {
                    failures.push(PreflightFailure::InputDigestMismatch {
                        path: input.staged_path.display().to_string(),
                        expected: input.sha256.to_hex(),
                        actual: actual.to_hex(),
                    });
                }
                Ok(_) => {}
            }
        }

        if plan.total_bytes * 10 < medium.free_bytes() {
            warnings.push("the image uses less than a tenth of the medium's capacity".to_owned());
        }

        Ok(PreflightReport {
            failures,
            warnings,
            medium: Some(medium),
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn write(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<WriteReport, EngineError> {
        // The engine re-runs preflight itself rather than trusting the caller.
        // A burn destroys media, so the last thing able to stop a mistake
        // should also try to.
        let preflight = self.preflight(plan).await?;
        if !preflight.passed() {
            return Err(EngineError::PreflightNotPassed);
        }
        self.check_cancelled()?;

        fs::create_dir_all(&self.media_root)
            .await
            .map_err(|source| EngineError::Io {
                operation: "preparing the simulated media directory",
                source,
            })?;

        let medium_path = self.medium_path(&plan.drive);
        let mut medium =
            fs::File::create(&medium_path)
                .await
                .map_err(|source| EngineError::Io {
                    operation: "opening the simulated medium",
                    source,
                })?;

        sink.emit(BurnEvent::new(
            "writing",
            "WRITE_STARTED",
            format!(
                "writing {} bytes to {}",
                plan.total_bytes, plan.drive.device_alias
            ),
        ));

        let mut written = 0_u64;
        let chunk_report_every = (plan.total_bytes / PROGRESS_CHUNKS).max(1);
        let mut next_report = chunk_report_every;

        for input in &plan.inputs {
            let mut source =
                fs::File::open(&input.staged_path)
                    .await
                    .map_err(|error| EngineError::Io {
                        operation: "opening a staged input",
                        source: error,
                    })?;

            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                self.check_cancelled().inspect_err(|_| {
                    sink.emit(BurnEvent::new(
                        "writing",
                        "WRITE_CANCELLED",
                        "cancelled mid-write; the medium is unusable",
                    ));
                })?;

                let read = source
                    .read(&mut buffer)
                    .await
                    .map_err(|source| EngineError::Io {
                        operation: "reading a staged input",
                        source,
                    })?;
                if read == 0 {
                    break;
                }

                if let Some(limit) = self.behaviour.fail_write_after_bytes
                    && written + read as u64 > limit
                {
                    // Write the bytes up to the failure point, then stop. A
                    // real failure leaves a partially written disc, and the
                    // simulation should too.
                    let partial = usize::try_from(limit.saturating_sub(written)).unwrap_or(0);
                    let _ = medium.write_all(&buffer[..partial]).await;
                    let _ = medium.flush().await;
                    sink.emit(BurnEvent::new(
                        "writing",
                        "WRITE_FAILED",
                        "simulated write failure",
                    ));
                    return Err(EngineError::WriteFailed {
                        bytes_written: limit,
                        reason: "simulated write failure".to_owned(),
                    });
                }

                medium
                    .write_all(&buffer[..read])
                    .await
                    .map_err(|source| EngineError::Io {
                        operation: "writing to the simulated medium",
                        source,
                    })?;
                written += read as u64;

                if written >= next_report {
                    next_report += chunk_report_every;
                    #[allow(clippy::cast_precision_loss)]
                    let fraction = written as f32 / plan.total_bytes.max(1) as f32;
                    sink.emit(BurnEvent::progress(
                        "writing",
                        "WRITE_PROGRESS",
                        fraction,
                        format!("{written} of {} bytes", plan.total_bytes),
                    ));
                }
            }
        }

        medium.flush().await.map_err(|source| EngineError::Io {
            operation: "flushing the simulated medium",
            source,
        })?;
        medium.sync_all().await.map_err(|source| EngineError::Io {
            operation: "syncing the simulated medium",
            source,
        })?;
        drop(medium);

        if self.behaviour.corrupt_after_write {
            // A drive that reports success while producing a bad disc. This is
            // the whole reason write success and verification are separate
            // states, so the simulation has to be able to produce it.
            corrupt(&medium_path).await?;
            sink.emit(BurnEvent::new(
                "writing",
                "WRITE_COMPLETED",
                "engine reported success",
            ));
        } else {
            sink.emit(BurnEvent::new(
                "writing",
                "WRITE_COMPLETED",
                "engine reported success",
            ));
        }

        Ok(WriteReport {
            engine_reported_success: true,
            bytes_written: written,
            engine: self.name().to_owned(),
            engine_version: self.version(),
            finalized: plan.finalize,
            diagnostics: vec![],
        })
    }

    async fn verify(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<VerifyReport, EngineError> {
        self.check_cancelled()?;
        sink.emit(BurnEvent::new(
            "verifying",
            "VERIFY_STARTED",
            "reading the medium back",
        ));

        let medium_path = self.medium_path(&plan.drive);
        let mut medium = fs::File::open(&medium_path)
            .await
            .map_err(|source| EngineError::Io {
                operation: "opening the simulated medium for verification",
                source,
            })?;

        let mut compared = 0_u64;
        let mut first_mismatch = None;

        // Compare the medium against the planned inputs in order, which is a
        // real byte-for-byte read-back rather than a recorded verdict.
        'inputs: for input in &plan.inputs {
            let mut source =
                fs::File::open(&input.staged_path)
                    .await
                    .map_err(|error| EngineError::Io {
                        operation: "opening a staged input for verification",
                        source: error,
                    })?;

            let mut expected = vec![0_u8; 64 * 1024];
            let mut actual = vec![0_u8; 64 * 1024];
            loop {
                self.check_cancelled()?;
                let want = source
                    .read(&mut expected)
                    .await
                    .map_err(|source| EngineError::Io {
                        operation: "reading a staged input",
                        source,
                    })?;
                if want == 0 {
                    break;
                }
                let got = read_exact_or_less(&mut medium, &mut actual[..want]).await?;

                if got < want || expected[..want] != actual[..want] {
                    let offset = first_difference(&expected[..want], &actual[..got])
                        .unwrap_or(got.min(want));
                    first_mismatch = Some(compared + offset as u64);
                    break 'inputs;
                }
                compared += want as u64;
            }
        }

        let matched = first_mismatch.is_none();
        sink.emit(BurnEvent::new(
            "verifying",
            if matched {
                "VERIFY_MATCHED"
            } else {
                "VERIFY_MISMATCH"
            },
            if matched {
                "read-back matched the source".to_owned()
            } else {
                "read-back did not match the source".to_owned()
            },
        ));

        Ok(VerifyReport {
            matched,
            bytes_compared: compared,
            method: "full_sector_readback".to_owned(),
            first_mismatch_offset: first_mismatch,
            // Always stated. A verification result that does not say what it
            // did not check invites being read as a stronger guarantee.
            limitations: vec![
                "compares written data against the source bytes only".to_owned(),
                "does not establish that any player, console or drive will \
                 accept the disc"
                    .to_owned(),
                "does not assess the physical longevity of the medium".to_owned(),
            ],
        })
    }

    async fn blank(
        &self,
        request: &BlankRequest,
        sink: &dyn EventSink,
    ) -> Result<BlankReport, EngineError> {
        // Refused without explicit confirmation. Erasing is destructive and
        // must never be a side effect of anything else.
        if !request.confirmed_destructive {
            return Err(EngineError::DestructiveNotConfirmed);
        }
        self.check_cancelled()?;

        sink.emit(BurnEvent::new(
            "blanking",
            "BLANK_STARTED",
            "erasing medium",
        ));
        let path = self.medium_path(&request.drive);
        let _ = fs::remove_file(&path).await;
        sink.emit(BurnEvent::new(
            "blanking",
            "BLANK_COMPLETED",
            "medium erased",
        ));

        Ok(BlankReport {
            erased: true,
            duration_seconds: if request.full { 90 } else { 1 },
            diagnostics: vec![],
        })
    }

    async fn eject(&self, drive: &DriveRef) -> Result<(), EngineError> {
        if self.behaviour.drive_unavailable {
            return Err(EngineError::DriveUnavailable {
                alias: drive.device_alias.clone(),
            });
        }
        Ok(())
    }
}

/// Read up to `buffer.len()` bytes, tolerating a short medium.
async fn read_exact_or_less(file: &mut fs::File, buffer: &mut [u8]) -> Result<usize, EngineError> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file
            .read(&mut buffer[filled..])
            .await
            .map_err(|source| EngineError::Io {
                operation: "reading the simulated medium",
                source,
            })?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

fn first_difference(expected: &[u8], actual: &[u8]) -> Option<usize> {
    expected
        .iter()
        .zip(actual.iter())
        .position(|(left, right)| left != right)
}

/// Flip a byte in the middle of the simulated medium.
async fn corrupt(path: &Path) -> Result<(), EngineError> {
    let mut bytes = fs::read(path).await.map_err(|source| EngineError::Io {
        operation: "reading the simulated medium to corrupt it",
        source,
    })?;
    if let Some(byte) = bytes.get_mut(0) {
        *byte = byte.wrapping_add(1);
    }
    fs::write(path, bytes)
        .await
        .map_err(|source| EngineError::Io {
            operation: "corrupting the simulated medium",
            source,
        })
}

/// Hash a file with SHA-256.
async fn hash_file(path: &Path) -> Result<tangible_domain::Sha256Digest, EngineError> {
    use sha2::{Digest as _, Sha256};

    let mut file = fs::File::open(path)
        .await
        .map_err(|source| EngineError::Io {
            operation: "opening a file to hash",
            source,
        })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|source| EngineError::Io {
                operation: "reading a file to hash",
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(tangible_domain::Sha256Digest::from_bytes(
        hasher.finalize().into(),
    ))
}
