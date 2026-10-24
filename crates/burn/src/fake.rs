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
    BlankReport, BlankRequest, BurnPlan, DriveCapabilities, DriveIdentity, DriveRef, MediumInfo,
    PreflightFailure, PreflightReport, RIP_DATA, RIP_IMAGE, RIP_TOC, RipReport, RipRequest,
    VerifyReport, WriteMode, WriteReport,
};

/// The table of contents a simulated rip writes: a data track and an audio
/// track in one file, as cdrdao writes for a mixed-mode disc.
pub const FAKE_RIP_TOC: &str = "CD_ROM\n\
TRACK MODE1\n\
DATAFILE \"disc.bin\" 00:01:00 // length in bytes: 153600\n\
TRACK AUDIO\n\
SILENCE 00:00:02\n\
FILE \"disc.bin\" #153600 0 00:00:25\n\
START 00:00:02\n";

/// The data a simulated rip writes: 75 data sectors and 25 audio sectors.
pub const FAKE_RIP_DATA_BYTES: usize = 75 * 2048 + 25 * 2352;

/// Blocks in the image a simulated DVD rip writes.
pub const FAKE_RIP_IMAGE_BLOCKS: u32 = 64;

/// A structurally valid ISO 9660 image: the system area, a primary volume
/// descriptor for `blocks` 2048-byte blocks, a terminator, then zeros.
#[must_use]
pub fn fake_iso_image(blocks: u32) -> Vec<u8> {
    const SECTOR: usize = 2048;
    let mut bytes = vec![0_u8; 16 * SECTOR];
    let mut pvd = vec![0_u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    for slot in &mut pvd[40..72] {
        *slot = b' ';
    }
    pvd[40..44].copy_from_slice(b"FAKE");
    pvd[80..84].copy_from_slice(&blocks.to_le_bytes());
    pvd[84..88].copy_from_slice(&blocks.to_be_bytes());
    pvd[128..130].copy_from_slice(&2048_u16.to_le_bytes());
    pvd[130..132].copy_from_slice(&2048_u16.to_be_bytes());
    bytes.extend_from_slice(&pvd);
    let mut terminator = vec![0_u8; SECTOR];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;
    bytes.extend_from_slice(&terminator);
    bytes.resize(blocks as usize * SECTOR, 0);
    bytes
}

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
    /// Fail a rip as a disc that does not read.
    pub rip_fails: bool,
    /// Take this many short steps over a rip, checking for cancellation
    /// between them, so a test can withdraw one mid-read.
    pub rip_steps: u32,
    /// Present a DVD that turns out to be CSS-encrypted when read.
    pub rip_encrypted: bool,
}

/// A decrypted DVD's `VIDEO_TS` files, as dvdbackup writes them: a video
/// manager naming one title set, the title set's information, and clear
/// video. Paths are relative to the copy's directory.
#[must_use]
pub fn fake_video_ts() -> Vec<(&'static str, Vec<u8>)> {
    let header = |signature: &[u8], title_sets: u16| {
        let mut bytes = vec![0_u8; 2048];
        bytes[..12].copy_from_slice(signature);
        bytes[0x3E..0x40].copy_from_slice(&title_sets.to_be_bytes());
        bytes
    };
    let mut vob = Vec::new();
    for _ in 0..70 {
        let mut sector = vec![0_u8; 2048];
        sector[..4].copy_from_slice(&[0, 0, 1, 0xBA]);
        sector[4] = 0x44;
        sector[13] = 0xF8;
        sector[14..18].copy_from_slice(&[0, 0, 1, 0xE0]);
        sector[20] = 0x80;
        vob.extend(sector);
    }
    vec![
        ("VIDEO_TS/VIDEO_TS.IFO", header(b"DVDVIDEO-VMG", 1)),
        ("VIDEO_TS/VIDEO_TS.BUP", header(b"DVDVIDEO-VMG", 1)),
        ("VIDEO_TS/VTS_01_0.IFO", header(b"DVDVIDEO-VTS", 0)),
        ("VIDEO_TS/VTS_01_0.BUP", header(b"DVDVIDEO-VTS", 0)),
        ("VIDEO_TS/VTS_01_1.VOB", vob),
    ]
}

/// A stand-in for dvdbackup: authentication that succeeds or fails as told,
/// and a copy that writes [`fake_video_ts`].
#[derive(Debug, Clone, Default)]
pub struct FakeCssCopier {
    /// Fail authentication, as a drive that will not authenticate.
    pub authenticate_fails: bool,
    /// Fail the copy.
    pub mirror_fails: bool,
    /// Write this file into the copy's `VIDEO_TS` as well.
    pub extra_file: Option<String>,
    /// Withdraw the rip as the copy starts, through the token given.
    pub cancel_during_mirror: bool,
}

#[async_trait]
impl crate::dvdbackup::CssCopier for FakeCssCopier {
    fn decryption(&self) -> tangible_domain::manifest::DecryptionRecord {
        tangible_domain::manifest::DecryptionRecord {
            system: "css".to_owned(),
            library: "libdvdcss".to_owned(),
            library_version: "1.6.0".to_owned(),
            tool: "dvdbackup".to_owned(),
            tool_version: "0.4.2-4.1".to_owned(),
        }
    }

    async fn authenticate(
        &self,
        _device: &Path,
        _work_dir: &Path,
        cancel: &CancelToken,
    ) -> Result<(), EngineError> {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        if self.authenticate_fails {
            return Err(EngineError::Unsupported {
                what: "dvdbackup could not read the disc's DVD-Video information; it exited 1"
                    .to_owned(),
            });
        }
        Ok(())
    }

    async fn mirror(
        &self,
        _device: &Path,
        into: &Path,
        _expected_bytes: Option<u64>,
        cancel: &CancelToken,
        sink: &dyn EventSink,
    ) -> Result<(), EngineError> {
        if self.cancel_during_mirror {
            cancel.cancel();
        }
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        if self.mirror_fails {
            return Err(EngineError::Unsupported {
                what: "dvdbackup could not copy the disc's video; it exited 1".to_owned(),
            });
        }
        let io = |source| EngineError::Io {
            operation: "writing the simulated copy",
            source,
        };
        fs::create_dir_all(into.join("VIDEO_TS"))
            .await
            .map_err(io)?;
        for (name, bytes) in fake_video_ts() {
            fs::write(into.join(name), bytes).await.map_err(io)?;
        }
        if let Some(extra) = &self.extra_file {
            fs::write(into.join("VIDEO_TS").join(extra), b"extra")
                .await
                .map_err(io)?;
        }
        sink.emit(BurnEvent::progress(
            "ripping",
            "RIP_COPY_PROGRESS",
            0.99,
            "99% of the decrypted copy made",
        ));
        Ok(())
    }
}

/// The CUE sheet a simulated redumper rip writes: a data track and an audio
/// track, each in its own file, as redumper splits a mixed-mode disc.
pub const FAKE_CUE: &str = "FILE \"disc (Track 1).bin\" BINARY\n\
  TRACK 01 MODE1/2352\n\
    INDEX 01 00:00:00\n\
FILE \"disc (Track 2).bin\" BINARY\n\
  TRACK 02 AUDIO\n\
    ISRC 000000000000\n\
    INDEX 00 00:00:00\n\
    INDEX 01 00:00:10\n";

/// A stand-in for redumper: writes [`FAKE_CUE`] and its two tracks, or fails
/// as told.
#[derive(Debug, Clone, Default)]
pub struct FakeCdDumper {
    /// Fail, as a disc with sectors that would not read.
    pub fails: bool,
    /// Withdraw the rip as the dump starts, through the token given.
    pub cancel_during_dump: bool,
    /// Report the drive as one redumper has no read offset for.
    pub offset_unknown: bool,
}

#[async_trait]
impl crate::redumper::CdDumper for FakeCdDumper {
    fn tool(&self) -> tangible_domain::manifest::ToolRef {
        tangible_domain::manifest::ToolRef {
            name: "redumper".to_owned(),
            version: "b768".to_owned(),
        }
    }

    async fn dump(
        &self,
        _device: &Path,
        output_dir: &Path,
        cancel: &CancelToken,
        sink: &dyn EventSink,
    ) -> Result<crate::redumper::CdDump, EngineError> {
        if self.cancel_during_dump {
            cancel.cancel();
        }
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        if self.fails {
            return Err(EngineError::Unsupported {
                what: "redumper found sectors that would not read, even after retrying; it \
                       exited 255"
                    .to_owned(),
            });
        }
        let io = |source| EngineError::Io {
            operation: "writing the simulated dump",
            source,
        };
        sink.emit(BurnEvent::progress(
            "ripping",
            "RIP_PROGRESS",
            0.5,
            "50% read",
        ));
        fs::write(output_dir.join("disc.cue"), FAKE_CUE)
            .await
            .map_err(io)?;
        fs::write(output_dir.join("disc (Track 1).bin"), vec![0_u8; 75 * 2352])
            .await
            .map_err(io)?;
        fs::write(output_dir.join("disc (Track 2).bin"), vec![0_u8; 35 * 2352])
            .await
            .map_err(io)?;
        // What redumper leaves beside them, which is never sent.
        fs::write(output_dir.join("disc.scram"), b"raw")
            .await
            .map_err(io)?;
        Ok(crate::redumper::CdDump {
            outputs: vec![
                "disc.cue".to_owned(),
                "disc (Track 1).bin".to_owned(),
                "disc (Track 2).bin".to_owned(),
            ],
            cd_read: tangible_domain::manifest::CdReadRecord {
                read_offset: (!self.offset_unknown).then_some(6),
                write_offset: Some(-6),
                generic_drive: true,
                retried_samples: 0,
            },
        })
    }
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
        let profile = self
            .behaviour
            .medium_profile
            .clone()
            .unwrap_or_else(|| "CD-R".to_owned());
        // The same rule the real engines use: a profile naming RW or RE.
        let rewritable = profile.contains("RW") || profile.contains("RE");
        MediumInfo {
            blank: self.behaviour.medium_sessions == 0,
            writable: !self.behaviour.medium_read_only,
            rewritable,
            capacity_blocks: blocks,
            free_blocks: blocks,
            block_size: 2048,
            manufacturer_id: Some("SIMULATED".to_owned()),
            sessions: self.behaviour.medium_sessions,
            erasable: rewritable,
            profile,
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
            // Plainly not hardware, so a report from a development stack
            // cannot be mistaken for one about a real drive.
            identity: DriveIdentity {
                vendor: Some("Tangible".to_owned()),
                model: Some("Fake drive".to_owned()),
                firmware: None,
            },
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
            tracks: Vec::new(),
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

    fn rips_images(&self) -> bool {
        true
    }

    async fn rip_image(
        &self,
        request: &RipRequest,
        sink: &dyn EventSink,
    ) -> Result<RipReport, EngineError> {
        if self.behaviour.drive_unavailable {
            return Err(EngineError::DriveUnavailable {
                alias: request.drive.device_alias.clone(),
            });
        }
        if self.behaviour.no_medium {
            return Err(EngineError::NoMedium {
                alias: request.drive.device_alias.clone(),
            });
        }
        if request.cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        if self.behaviour.rip_encrypted
            && request.scrambled == crate::block_reader::Scrambled::Refuse
        {
            return Err(EngineError::Encrypted { system: "CSS" });
        }
        if self.behaviour.rip_fails {
            return Err(EngineError::Unsupported {
                what: "the simulated disc did not read".to_owned(),
            });
        }
        fs::create_dir_all(&request.output_dir)
            .await
            .map_err(|source| EngineError::Io {
                operation: "creating the rip directory",
                source,
            })?;
        let image = fake_iso_image(FAKE_RIP_IMAGE_BLOCKS);
        fs::write(request.output_dir.join(RIP_IMAGE), &image)
            .await
            .map_err(|source| EngineError::Io {
                operation: "writing the simulated image",
                source,
            })?;
        sink.emit(BurnEvent::progress(
            "ripping",
            "RIP_PROGRESS",
            1.0,
            "100% read",
        ));
        Ok(RipReport {
            engine: "fake".to_owned(),
            engine_version: self.version(),
            unreadable_sectors: 0,
            retried_sectors: 0,
            data_bytes: image.len() as u64,
        })
    }

    fn rips_cds(&self) -> bool {
        true
    }

    async fn css_encrypted(&self, drive: &DriveRef) -> Result<bool, EngineError> {
        if self.behaviour.drive_unavailable {
            return Err(EngineError::DriveUnavailable {
                alias: drive.device_alias.clone(),
            });
        }
        Ok(self.behaviour.rip_encrypted)
    }

    async fn rip_cd(
        &self,
        request: &RipRequest,
        sink: &dyn EventSink,
    ) -> Result<RipReport, EngineError> {
        if self.behaviour.drive_unavailable {
            return Err(EngineError::DriveUnavailable {
                alias: request.drive.device_alias.clone(),
            });
        }
        if self.behaviour.no_medium {
            return Err(EngineError::NoMedium {
                alias: request.drive.device_alias.clone(),
            });
        }
        let io = |operation: &'static str| {
            move |source: std::io::Error| EngineError::Io { operation, source }
        };
        fs::create_dir_all(&request.output_dir)
            .await
            .map_err(io("creating the rip directory"))?;
        sink.emit(BurnEvent::new(
            "ripping",
            "RIP_STARTED",
            "reading the simulated disc",
        ));
        let steps = self.behaviour.rip_steps.max(1);
        for step in 1..=steps {
            if request.cancel.is_cancelled() {
                for name in [RIP_TOC, RIP_DATA] {
                    let _ = fs::remove_file(request.output_dir.join(name)).await;
                }
                return Err(EngineError::Cancelled);
            }
            sink.emit(BurnEvent::progress(
                "ripping",
                "RIP_PROGRESS",
                // Steps are few: a test's count, at most a few hundred.
                #[allow(clippy::cast_precision_loss)]
                {
                    step as f32 / steps as f32
                },
                format!("read {step} of {steps}"),
            ));
            if self.behaviour.rip_steps > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        if self.behaviour.rip_fails {
            return Err(EngineError::Unsupported {
                what: "the simulated disc did not read".to_owned(),
            });
        }
        fs::write(request.output_dir.join(RIP_TOC), FAKE_RIP_TOC)
            .await
            .map_err(io("writing the simulated table of contents"))?;
        fs::write(
            request.output_dir.join(RIP_DATA),
            vec![0_u8; FAKE_RIP_DATA_BYTES],
        )
        .await
        .map_err(io("writing the simulated data"))?;
        Ok(RipReport {
            engine: "fake".to_owned(),
            engine_version: self.version(),
            unreadable_sectors: 0,
            retried_sectors: 0,
            data_bytes: FAKE_RIP_DATA_BYTES as u64,
        })
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
