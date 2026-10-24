// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ripping the disc in the drive, once an operator has asked for it
//! (ADR 0013).
//!
//! As with an erasure, the request names the drive and only the worker can
//! see what is in it, so the decision is made here: an empty drive, a blank
//! disc and a Blu-ray are refused, each as what it was. A CD is read by
//! cdrdao into a table of contents and its data; a DVD is read sector by
//! sector into an image, and refused if it turns out to be encrypted.
//! Reading changes nothing on the disc, so a rip that is withdrawn simply
//! stops.
//!
//! # Decrypting
//!
//! A rip asked to decrypt a DVD, on a worker that can, first offers the
//! drive authentication ([`CssCopier::authenticate`]), then asks whether the
//! disc is CSS-encrypted at all. One that is not is read as any other. One
//! that is becomes what the operator chose:
//!
//! - the encrypted original (the default): the image read sector by sector,
//!   scrambled sectors kept as they are on the disc, and a decrypted copy of
//!   its video under `decrypted/VIDEO_TS/`. Both in one rip, because the
//!   authentication lasts only while the disc stays in the drive;
//! - a decrypted original: only the decrypted `VIDEO_TS` tree.
//!
//! The completion then says the disc was `css` and names the library and
//! tool that decrypted it. Never a key: there is none here to name.
//!
//! # Reading a CD with its read offset corrected
//!
//! On a worker that has redumper (ADR 0015), a CD is read with it instead of
//! cdrdao, unless the rip asked for cdrdao: the CUE sheet and track files it
//! writes are sent, and the completion records the offsets. A rip that
//! insisted on redumper, on a worker without it, reads nothing.

use std::path::Path;

use tangible_domain::RipOriginal;
use tangible_domain::manifest::DecryptionRecord;
use tangible_domain::rip::{
    CD_ENGINE_UNAVAILABLE, DECRYPTED_PREFIX, VIDEO_TS_DIR, video_ts_member,
};

use crate::block_reader::Scrambled;
use crate::client::{ReadErrorsBody, RipCompletion};
use crate::dvdbackup::CssCopier;
use crate::engine::{BurnEngine, BurnEvent, EngineError, EventSink};
use crate::fake::CancelToken;
use crate::plan::{DriveRef, MediumInfo, RIP_DATA, RIP_IMAGE, RIP_TOC, RipReport, RipRequest};
use crate::redumper::CdDumper;

/// The directory a decrypted copy is made in, beside the encrypted image.
const COPY_DIR: &str = "decrypted";

/// What to do with the disc found in the drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipDecision {
    /// A CD with something on it: read it into a table of contents and data.
    Cd,
    /// A DVD with something on it: read it into an image.
    Image,
    /// Nothing to rip, with a stable code and why.
    Refuse {
        /// Such as `BLANK_MEDIUM`.
        code: &'static str,
        /// In words an operator can act on.
        detail: String,
    },
}

/// Decide from the drive's report.
///
/// The profile is how the drive names the disc: every CD profile starts
/// `CD` (`CD-ROM`, `CD-R`, `CD-RW`) and every DVD profile `DVD`. Blu-rays
/// wait for encryption detection that covers them (AACS), and are refused
/// rather than read without it.
#[must_use]
pub fn rip_decision(medium: &MediumInfo) -> RipDecision {
    if medium.blank {
        return RipDecision::Refuse {
            code: "BLANK_MEDIUM",
            detail: format!("the {} in the drive is blank", medium.profile),
        };
    }
    let profile = medium.profile.to_ascii_uppercase();
    if profile.starts_with("CD") {
        return RipDecision::Cd;
    }
    if profile.starts_with("DVD") {
        return RipDecision::Image;
    }
    RipDecision::Refuse {
        code: "MEDIUM_UNSUPPORTED",
        detail: format!(
            "the disc in the drive is {}; only CDs and DVDs can be ripped so far",
            medium.profile
        ),
    }
}

/// What a rip was asked for beyond reading the disc.
#[derive(Clone, Copy, Default)]
pub struct RipOptions<'a> {
    /// Set when the rip asked to decrypt and this worker can.
    pub decrypt: Option<Decrypting<'a>>,
    /// Whether this worker can decrypt at all, for what a refusal says.
    pub can_decrypt: bool,
    /// What reads the disc if it is a CD.
    pub cd: CdReader<'a>,
}

/// What reads a CD (ADR 0015).
#[derive(Clone, Copy, Default)]
pub enum CdReader<'a> {
    /// The engine's own reader, cdrdao: the read offset is not corrected.
    #[default]
    Engine,
    /// redumper, the read offset corrected.
    Corrected(&'a dyn CdDumper),
    /// The rip insisted on redumper, and this worker has none: a CD is
    /// refused with nothing read, never read with something else.
    Unavailable,
}

/// How a rip that may decrypt does it.
#[derive(Clone, Copy)]
pub struct Decrypting<'a> {
    /// The original the operator chose.
    pub keep: RipOriginal,
    /// What decrypts.
    pub copier: &'a dyn CssCopier,
}

/// How a rip went.
#[derive(Debug, Clone, PartialEq)]
pub enum RipOutcome {
    /// It ended, one way or another, and the server should hear how.
    Ended(Box<RipCompletion>),
    /// It was withdrawn. The server already knows; nothing to report.
    Withdrawn,
}

fn ended(outcome: &str, medium: Option<MediumInfo>, code: &str, detail: String) -> RipOutcome {
    RipOutcome::Ended(Box::new(RipCompletion {
        outcome: outcome.to_owned(),
        engine: None,
        engine_version: None,
        medium,
        read_errors: None,
        outputs: Vec::new(),
        encryption: None,
        decryption: None,
        cd_read: None,
        error_code: Some(code.to_owned()),
        error_detail: Some(detail),
    }))
}

/// A read that finished.
fn read(medium: MediumInfo, report: RipReport, outputs: Vec<String>) -> RipCompletion {
    RipCompletion {
        outcome: "read".to_owned(),
        engine: Some(report.engine),
        engine_version: Some(report.engine_version),
        medium: Some(medium),
        read_errors: Some(ReadErrorsBody {
            unreadable_sectors: report.unreadable_sectors,
            retried_sectors: report.retried_sectors,
        }),
        outputs,
        encryption: None,
        decryption: None,
        cd_read: None,
        error_code: None,
        error_detail: None,
    }
}

/// Look in the drive and read the disc into `output_dir`, if it should be.
///
/// Never fails: every ending is an outcome, because an operator waiting on a
/// rip needs to hear how it ended. On a read, the directory holds
/// [`RIP_TOC`] and [`RIP_DATA`], [`RIP_IMAGE`], or a `VIDEO_TS` tree, and the
/// completion names them; the caller sends them before reporting it.
pub async fn rip_disc<E: BurnEngine + ?Sized>(
    engine: &E,
    drive: &DriveRef,
    output_dir: &Path,
    options: RipOptions<'_>,
    cancel: &CancelToken,
    sink: &dyn EventSink,
) -> RipOutcome {
    let medium = match engine.inspect_medium(drive).await {
        Ok(medium) => medium,
        Err(EngineError::NoMedium { .. }) => {
            return ended(
                "refused",
                None,
                "NO_MEDIUM",
                "there is no disc in the drive".to_owned(),
            );
        }
        Err(EngineError::UnsuitableMedium { reason }) => {
            return ended("refused", None, "UNSUITABLE_MEDIUM", reason);
        }
        Err(error) => return ended("failed", None, "DRIVE_UNAVAILABLE", error.to_string()),
    };
    let decision = rip_decision(&medium);
    if let RipDecision::Refuse { code, detail } = decision {
        return ended("refused", Some(medium), code, detail);
    }

    let request = RipRequest {
        drive: drive.clone(),
        output_dir: output_dir.to_path_buf(),
        cancel: cancel.clone(),
        expected_bytes: (medium.capacity_blocks > 0).then(|| {
            medium
                .capacity_blocks
                .saturating_mul(u64::from(medium.block_size))
        }),
        scrambled: Scrambled::Refuse,
    };
    if decision == RipDecision::Cd {
        return match options.cd {
            CdReader::Engine => match engine.rip_cd(&request, sink).await {
                Ok(report) => RipOutcome::Ended(Box::new(read(
                    medium,
                    report,
                    vec![RIP_TOC.to_owned(), RIP_DATA.to_owned()],
                ))),
                Err(error) => read_failed(error, medium, options.can_decrypt),
            },
            CdReader::Corrected(dumper) => {
                rip_cd_corrected(dumper, &request, medium, options.can_decrypt, sink).await
            }
            CdReader::Unavailable => ended(
                "refused",
                Some(medium),
                CD_ENGINE_UNAVAILABLE,
                "this rip asked for redumper, and this worker does not have it; nothing was \
                 read. Ask again with cdrdao, or run the worker from the image built with \
                 redumper"
                    .to_owned(),
            ),
        };
    }
    if let Some(decrypting) = options.decrypt {
        return rip_dvd_decrypting(engine, request, medium, decrypting, sink).await;
    }
    rip_plain_image(engine, &request, medium, options.can_decrypt, sink).await
}

/// Read a CD with redumper into a CUE sheet and its tracks.
async fn rip_cd_corrected(
    dumper: &dyn CdDumper,
    request: &RipRequest,
    medium: MediumInfo,
    can_decrypt: bool,
    sink: &dyn EventSink,
) -> RipOutcome {
    if let Err(source) = tokio::fs::create_dir_all(&request.output_dir).await {
        return ended(
            "failed",
            Some(medium),
            "READ_FAILED",
            EngineError::Io {
                operation: "creating the rip directory",
                source,
            }
            .to_string(),
        );
    }
    let device = Path::new(&request.drive.device_alias);
    match dumper
        .dump(device, &request.output_dir, &request.cancel, sink)
        .await
    {
        Ok(dump) => {
            let tool = dumper.tool();
            sink.emit(BurnEvent::new(
                "ripping",
                "RIP_READ",
                match dump.cd_read.read_offset {
                    Some(offset) => format!("read, the drive's read offset ({offset:+}) corrected"),
                    None => "read; redumper has no read offset for this drive, so none was \
                             corrected"
                        .to_owned(),
                },
            ));
            RipOutcome::Ended(Box::new(RipCompletion {
                outcome: "read".to_owned(),
                engine: Some(tool.name),
                engine_version: Some(tool.version),
                medium: Some(medium),
                // redumper counts samples, not sectors; what it read again
                // is in the offsets record, and a rip that finished read
                // every sector in the end.
                read_errors: None,
                outputs: dump.outputs,
                encryption: None,
                decryption: None,
                cd_read: Some(dump.cd_read),
                error_code: None,
                error_detail: None,
            }))
        }
        Err(error) => read_failed(error, medium, can_decrypt),
    }
}

/// Read a DVD or Blu-ray into one image, refusing it if it is encrypted.
async fn rip_plain_image<E: BurnEngine + ?Sized>(
    engine: &E,
    request: &RipRequest,
    medium: MediumInfo,
    can_decrypt: bool,
    sink: &dyn EventSink,
) -> RipOutcome {
    match engine.rip_image(request, sink).await {
        Ok(report) => RipOutcome::Ended(Box::new(read(medium, report, vec![RIP_IMAGE.to_owned()]))),
        Err(error) => read_failed(error, medium, can_decrypt),
    }
}

/// Rip a DVD for a rip asked to decrypt it. See the module documentation.
async fn rip_dvd_decrypting<E: BurnEngine + ?Sized>(
    engine: &E,
    request: RipRequest,
    medium: MediumInfo,
    decrypting: Decrypting<'_>,
    sink: &dyn EventSink,
) -> RipOutcome {
    let device = Path::new(&request.drive.device_alias);
    if let Err(source) = tokio::fs::create_dir_all(&request.output_dir).await {
        return ended(
            "failed",
            Some(medium),
            "READ_FAILED",
            EngineError::Io {
                operation: "creating the rip directory",
                source,
            }
            .to_string(),
        );
    }

    // Offered whether or not the disc turns out to need it: until the drive
    // has authenticated, a CSS disc's video may not read at all, and asking
    // first is what tells an encrypted disc from a damaged one.
    sink.emit(BurnEvent::new(
        "ripping",
        "RIP_AUTHENTICATING",
        "asking the drive to authenticate the disc, in case it is encrypted",
    ));
    let authenticated = match decrypting
        .copier
        .authenticate(device, &request.output_dir, &request.cancel)
        .await
    {
        Err(EngineError::Cancelled) => return RipOutcome::Withdrawn,
        other => other,
    };
    let encrypted = match engine.css_encrypted(&request.drive).await {
        Ok(encrypted) => encrypted,
        Err(error) => return ended("failed", Some(medium), "READ_FAILED", error.to_string()),
    };
    if !encrypted {
        sink.emit(BurnEvent::new(
            "ripping",
            "RIP_NOT_ENCRYPTED",
            "the disc is not encrypted; it is read as it is, with nothing to decrypt",
        ));
        let mut outcome = rip_plain_image(engine, &request, medium, true, sink).await;
        if let RipOutcome::Ended(completion) = &mut outcome
            && completion.outcome == "read"
        {
            completion.encryption = Some("none".to_owned());
        }
        return outcome;
    }
    if let Err(error) = authenticated {
        return decryption_failed(&error, medium, "the drive could not be authenticated");
    }
    sink.emit(BurnEvent::new(
        "ripping",
        "RIP_AUTHENTICATED",
        "the disc is CSS-encrypted, and the drive has authenticated it",
    ));

    let record = decrypting.copier.decryption();
    let completion = match decrypting.keep {
        RipOriginal::EncryptedOriginal => {
            keep_encrypted_original(engine, &request, medium, decrypting.copier, sink).await
        }
        RipOriginal::DecryptedOriginal => {
            keep_decrypted_original(&request, medium, decrypting.copier, &record, sink).await
        }
    };
    match completion {
        Ok(completion) => RipOutcome::Ended(Box::new(RipCompletion {
            encryption: Some("css".to_owned()),
            decryption: Some(record),
            ..completion
        })),
        Err(outcome) => *outcome,
    }
}

/// The disc as it is, scrambled sectors and all, then a decrypted copy of
/// its video beside it, while the drive is still authenticated.
async fn keep_encrypted_original<E: BurnEngine + ?Sized>(
    engine: &E,
    request: &RipRequest,
    medium: MediumInfo,
    copier: &dyn CssCopier,
    sink: &dyn EventSink,
) -> Result<RipCompletion, Box<RipOutcome>> {
    let image = RipRequest {
        scrambled: Scrambled::Keep,
        ..request.clone()
    };
    let report = match engine.rip_image(&image, sink).await {
        Ok(report) => report,
        Err(error) => return Err(Box::new(read_failed(error, medium, true))),
    };
    let device = Path::new(&request.drive.device_alias);
    let copy_dir = request.output_dir.join(COPY_DIR);
    make_copy(copier, device, &copy_dir, request, &medium, sink).await?;
    let copy = match video_ts_outputs(&copy_dir, DECRYPTED_PREFIX).await {
        Ok(copy) => copy,
        Err(detail) => {
            return Err(Box::new(ended(
                "failed",
                Some(medium),
                "DECRYPTION_FAILED",
                detail,
            )));
        }
    };
    let mut outputs = vec![RIP_IMAGE.to_owned()];
    outputs.extend(copy);
    Ok(read(medium, report, outputs))
}

/// Only the decrypted `VIDEO_TS` tree.
async fn keep_decrypted_original(
    request: &RipRequest,
    medium: MediumInfo,
    copier: &dyn CssCopier,
    record: &DecryptionRecord,
    sink: &dyn EventSink,
) -> Result<RipCompletion, Box<RipOutcome>> {
    let device = Path::new(&request.drive.device_alias);
    make_copy(copier, device, &request.output_dir, request, &medium, sink).await?;
    let outputs = match video_ts_outputs(&request.output_dir, "").await {
        Ok(outputs) => outputs,
        Err(detail) => {
            return Err(Box::new(ended(
                "failed",
                Some(medium),
                "DECRYPTION_FAILED",
                detail,
            )));
        }
    };
    Ok(RipCompletion {
        outcome: "read".to_owned(),
        engine: Some(record.tool.clone()),
        engine_version: Some(record.tool_version.clone()),
        medium: Some(medium),
        // dvdbackup does not say; it stops at a sector it cannot read
        // rather than skipping it.
        read_errors: None,
        outputs,
        encryption: None,
        decryption: None,
        cd_read: None,
        error_code: None,
        error_detail: None,
    })
}

/// Make the decrypted copy in `into`, or say how the rip ended instead.
async fn make_copy(
    copier: &dyn CssCopier,
    device: &Path,
    into: &Path,
    request: &RipRequest,
    medium: &MediumInfo,
    sink: &dyn EventSink,
) -> Result<(), Box<RipOutcome>> {
    if let Err(source) = tokio::fs::create_dir_all(into).await {
        return Err(Box::new(ended(
            "failed",
            Some(medium.clone()),
            "READ_FAILED",
            EngineError::Io {
                operation: "creating the copy's directory",
                source,
            }
            .to_string(),
        )));
    }
    sink.emit(BurnEvent::new(
        "ripping",
        "RIP_DECRYPTING",
        "making a decrypted copy of the disc's video",
    ));
    match copier
        .mirror(device, into, request.expected_bytes, &request.cancel, sink)
        .await
    {
        Ok(()) => {
            sink.emit(BurnEvent::new(
                "ripping",
                "RIP_DECRYPTED",
                "the decrypted copy is made",
            ));
            Ok(())
        }
        Err(EngineError::Cancelled) => Err(Box::new(RipOutcome::Withdrawn)),
        Err(error) => Err(Box::new(decryption_failed(
            &error,
            medium.clone(),
            "the decrypted copy could not be made",
        ))),
    }
}

fn decryption_failed(error: &EngineError, medium: MediumInfo, what: &str) -> RipOutcome {
    let why = match error {
        EngineError::Unsupported { what } => what.clone(),
        EngineError::Timeout { seconds, .. } => {
            format!("dvdbackup did not finish within {seconds} seconds")
        }
        other => other.to_string(),
    };
    ended(
        "failed",
        Some(medium),
        "DECRYPTION_FAILED",
        format!("the disc is CSS-encrypted, and {what}: {why}; nothing that was read is kept"),
    )
}

/// The files of the `VIDEO_TS` tree in `dir`, as the names they are sent
/// under, each with `prefix`.
///
/// Every entry must be a regular file a DVD-Video disc has: a name the
/// server would refuse is refused here, saying which, rather than sent.
async fn video_ts_outputs(dir: &Path, prefix: &str) -> Result<Vec<String>, String> {
    let tree = dir.join(VIDEO_TS_DIR);
    let mut entries = tokio::fs::read_dir(&tree)
        .await
        .map_err(|error| format!("the decrypted copy has no VIDEO_TS: {error}"))?;
    let mut names = Vec::new();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => return Err(format!("the decrypted copy could not be listed: {error}")),
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err("the decrypted copy holds a file whose name is not text".to_owned());
        };
        let relative = format!("{VIDEO_TS_DIR}{name}");
        let is_file = entry.file_type().await.is_ok_and(|kind| kind.is_file());
        if !is_file || video_ts_member(&relative).is_none() {
            return Err(format!(
                "the decrypted copy holds {name}, which is not a DVD-Video file"
            ));
        }
        names.push(format!("{prefix}{relative}"));
    }
    names.sort_unstable();
    if !names
        .iter()
        .any(|name| name.ends_with("VIDEO_TS/VIDEO_TS.IFO"))
    {
        return Err("the decrypted copy has no VIDEO_TS.IFO".to_owned());
    }
    Ok(names)
}

/// How a read that did not finish ended.
fn read_failed(error: EngineError, medium: MediumInfo, can_decrypt: bool) -> RipOutcome {
    match error {
        EngineError::Cancelled => RipOutcome::Withdrawn,
        EngineError::UnsuitableMedium { reason } => {
            ended("refused", Some(medium), "RELOAD_DISC", reason)
        }
        EngineError::Encrypted { system } => ended(
            "refused",
            Some(medium),
            "ENCRYPTED",
            if can_decrypt {
                format!(
                    "the disc is encrypted ({system}), and this rip did not ask for it to be \
                     decrypted; nothing that was read is kept. Ask again with decryption to rip it"
                )
            } else {
                format!(
                    "the disc is encrypted ({system}); this worker does not rip encrypted discs, \
                     and nothing that was read is kept"
                )
            },
        ),
        EngineError::NoMedium { .. } => ended(
            "refused",
            Some(medium),
            "NO_MEDIUM",
            "the disc was taken out".to_owned(),
        ),
        EngineError::Timeout { seconds, .. } => ended(
            "failed",
            Some(medium),
            "READ_TIMEOUT",
            format!("the disc did not finish reading within {seconds} seconds"),
        ),
        error @ EngineError::DriveUnavailable { .. } => ended(
            "failed",
            Some(medium),
            "DRIVE_UNAVAILABLE",
            error.to_string(),
        ),
        EngineError::Unsupported { what } => ended("failed", Some(medium), "READ_FAILED", what),
        error => ended("failed", Some(medium), "READ_FAILED", error.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::engine::CollectingSink;
    use crate::fake::{FakeBehaviour, FakeCdDumper, FakeCssCopier, FakeEngine};

    fn drive() -> DriveRef {
        DriveRef {
            worker_id: tangible_domain::WorkerId::generate(),
            drive_id: tangible_domain::DriveId::generate(),
            device_alias: "/dev/disc-block".to_owned(),
        }
    }

    fn engine(behaviour: FakeBehaviour) -> (tempfile::TempDir, FakeEngine) {
        let dir = tempfile::tempdir().unwrap();
        let engine = FakeEngine::new(dir.path()).with_behaviour(behaviour);
        (dir, engine)
    }

    fn pressed_cd() -> FakeBehaviour {
        FakeBehaviour {
            medium_profile: Some("CD-ROM".to_owned()),
            medium_sessions: 1,
            ..FakeBehaviour::default()
        }
    }

    async fn rip(engine: &FakeEngine, out: &Path) -> RipOutcome {
        rip_disc(
            engine,
            &drive(),
            out,
            RipOptions::default(),
            &CancelToken::new(),
            &CollectingSink::new(),
        )
        .await
    }

    #[tokio::test]
    async fn a_cd_is_read_into_its_two_files() {
        let (dir, engine) = engine(pressed_cd());
        let out = dir.path().join("rip");
        let RipOutcome::Ended(done) = rip(&engine, &out).await else {
            panic!("withdrawn")
        };
        assert_eq!(done.outcome, "read", "{done:?}");
        assert_eq!(done.outputs, ["disc.toc", "disc.bin"]);
        assert_eq!(done.engine.as_deref(), Some("fake"));
        assert!(out.join("disc.toc").is_file() && out.join("disc.bin").is_file());
        assert_eq!(done.medium.unwrap().profile, "CD-ROM");
    }

    #[tokio::test]
    async fn an_empty_drive_a_blank_disc_a_blu_ray_and_an_encrypted_dvd_are_refused() {
        for (behaviour, code) in [
            (
                FakeBehaviour {
                    no_medium: true,
                    ..FakeBehaviour::default()
                },
                "UNSUITABLE_MEDIUM",
            ),
            (FakeBehaviour::default(), "BLANK_MEDIUM"),
            (
                FakeBehaviour {
                    medium_profile: Some("BD-ROM".to_owned()),
                    medium_sessions: 1,
                    ..FakeBehaviour::default()
                },
                "MEDIUM_UNSUPPORTED",
            ),
            (
                FakeBehaviour {
                    medium_profile: Some("DVD-ROM".to_owned()),
                    medium_sessions: 1,
                    rip_encrypted: true,
                    ..FakeBehaviour::default()
                },
                "ENCRYPTED",
            ),
        ] {
            let (dir, engine) = engine(behaviour);
            let out = dir.path().join("rip");
            let RipOutcome::Ended(done) = rip(&engine, &out).await else {
                panic!("withdrawn")
            };
            assert_eq!(done.outcome, "refused");
            assert_eq!(done.error_code.as_deref(), Some(code));
            assert!(!out.join("disc.bin").exists(), "nothing was read");
        }
    }

    #[tokio::test]
    async fn a_dvd_is_read_into_one_image() {
        let (dir, engine) = engine(FakeBehaviour {
            medium_profile: Some("DVD-RW sequential recording".to_owned()),
            medium_sessions: 1,
            ..FakeBehaviour::default()
        });
        let out = dir.path().join("rip");
        let RipOutcome::Ended(done) = rip(&engine, &out).await else {
            panic!("withdrawn")
        };
        assert_eq!(done.outcome, "read", "{done:?}");
        assert_eq!(done.outputs, ["disc.iso"]);
        assert!(out.join("disc.iso").is_file());
        assert!(!out.join("disc.bin").exists());
    }

    #[tokio::test]
    async fn a_disc_that_does_not_read_fails() {
        let (dir, engine) = engine(FakeBehaviour {
            rip_fails: true,
            ..pressed_cd()
        });
        let RipOutcome::Ended(done) = rip(&engine, &dir.path().join("rip")).await else {
            panic!("withdrawn")
        };
        assert_eq!(done.outcome, "failed");
        assert_eq!(done.error_code.as_deref(), Some("READ_FAILED"));
    }

    #[tokio::test]
    async fn a_withdrawn_rip_stops_and_reports_nothing() {
        let (dir, engine) = engine(FakeBehaviour {
            rip_steps: 20,
            ..pressed_cd()
        });
        let cancel = CancelToken::new();
        let out = dir.path().join("rip");
        let stopper = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                cancel.cancel();
            })
        };
        let outcome = rip_disc(
            &engine,
            &drive(),
            &out,
            RipOptions::default(),
            &cancel,
            &CollectingSink::new(),
        )
        .await;
        stopper.await.unwrap();
        assert_eq!(outcome, RipOutcome::Withdrawn);
        assert!(!out.join("disc.bin").exists());
    }

    fn encrypted_dvd() -> FakeBehaviour {
        FakeBehaviour {
            medium_profile: Some("DVD-ROM".to_owned()),
            medium_sessions: 1,
            rip_encrypted: true,
            ..FakeBehaviour::default()
        }
    }

    async fn rip_decrypting(
        engine: &FakeEngine,
        out: &Path,
        keep: RipOriginal,
        copier: &FakeCssCopier,
    ) -> (RipOutcome, CollectingSink) {
        let sink = CollectingSink::new();
        let outcome = rip_disc(
            engine,
            &drive(),
            out,
            RipOptions {
                decrypt: Some(Decrypting { keep, copier }),
                can_decrypt: true,
                ..RipOptions::default()
            },
            &CancelToken::new(),
            &sink,
        )
        .await;
        (outcome, sink)
    }

    fn codes(sink: &CollectingSink) -> Vec<String> {
        sink.events()
            .iter()
            .map(|event| event.code.to_owned())
            .collect()
    }

    #[tokio::test]
    async fn an_encrypted_original_is_the_image_with_a_decrypted_copy_beside_it() {
        let (dir, engine) = engine(encrypted_dvd());
        let out = dir.path().join("rip");
        let (outcome, sink) = rip_decrypting(
            &engine,
            &out,
            RipOriginal::EncryptedOriginal,
            &FakeCssCopier::default(),
        )
        .await;
        let RipOutcome::Ended(done) = outcome else {
            panic!("withdrawn")
        };
        assert_eq!(done.outcome, "read", "{done:?}");
        assert_eq!(
            done.outputs,
            [
                "disc.iso",
                "decrypted/VIDEO_TS/VIDEO_TS.BUP",
                "decrypted/VIDEO_TS/VIDEO_TS.IFO",
                "decrypted/VIDEO_TS/VTS_01_0.BUP",
                "decrypted/VIDEO_TS/VTS_01_0.IFO",
                "decrypted/VIDEO_TS/VTS_01_1.VOB",
            ]
        );
        for name in &done.outputs {
            assert!(out.join(name).is_file(), "{name}");
        }
        assert_eq!(done.engine.as_deref(), Some("fake"), "the image's reader");
        assert_eq!(done.encryption.as_deref(), Some("css"));
        let decryption = done.decryption.clone().unwrap();
        assert_eq!(
            (decryption.system.as_str(), decryption.library.as_str()),
            ("css", "libdvdcss")
        );
        let codes = codes(&sink);
        let at = |code: &str| codes.iter().position(|c| c == code).unwrap();
        assert!(at("RIP_AUTHENTICATING") < at("RIP_AUTHENTICATED"));
        assert!(at("RIP_PROGRESS") < at("RIP_DECRYPTING"), "image first");
        assert!(at("RIP_DECRYPTING") < at("RIP_DECRYPTED"));
        // What is sent is what the server takes from such a rip.
        assert!(
            tangible_domain::rip::check_output_set(
                &done.outputs,
                true,
                Some(RipOriginal::EncryptedOriginal)
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn a_decrypted_original_is_only_the_tree() {
        let (dir, engine) = engine(encrypted_dvd());
        let out = dir.path().join("rip");
        let (outcome, _) = rip_decrypting(
            &engine,
            &out,
            RipOriginal::DecryptedOriginal,
            &FakeCssCopier::default(),
        )
        .await;
        let RipOutcome::Ended(done) = outcome else {
            panic!("withdrawn")
        };
        assert_eq!(done.outcome, "read", "{done:?}");
        assert_eq!(done.outputs.len(), 5);
        assert!(
            done.outputs
                .iter()
                .all(|name| name.starts_with("VIDEO_TS/"))
        );
        assert!(!out.join("disc.iso").exists(), "no image read");
        assert_eq!(done.engine.as_deref(), Some("dvdbackup"));
        assert_eq!(done.engine_version.as_deref(), Some("0.4.2-4.1"));
        assert_eq!(done.encryption.as_deref(), Some("css"));
        assert!(done.decryption.is_some());
        assert!(
            tangible_domain::rip::check_output_set(
                &done.outputs,
                true,
                Some(RipOriginal::DecryptedOriginal)
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn a_disc_that_is_not_encrypted_is_read_as_it_is() {
        let (dir, engine) = engine(FakeBehaviour {
            rip_encrypted: false,
            ..encrypted_dvd()
        });
        let out = dir.path().join("rip");
        for keep in [
            RipOriginal::EncryptedOriginal,
            RipOriginal::DecryptedOriginal,
        ] {
            let _ = std::fs::remove_dir_all(&out);
            // Authentication failing, as on a data DVD, does not matter.
            let copier = FakeCssCopier {
                authenticate_fails: true,
                ..FakeCssCopier::default()
            };
            let (outcome, sink) = rip_decrypting(&engine, &out, keep, &copier).await;
            let RipOutcome::Ended(done) = outcome else {
                panic!("withdrawn")
            };
            assert_eq!(done.outcome, "read", "{done:?}");
            assert_eq!(done.outputs, ["disc.iso"]);
            assert_eq!(done.encryption.as_deref(), Some("none"));
            assert!(done.decryption.is_none());
            assert!(codes(&sink).contains(&"RIP_NOT_ENCRYPTED".to_owned()));
        }
    }

    #[tokio::test]
    async fn decryption_that_fails_fails_the_rip_and_says_why() {
        for copier in [
            FakeCssCopier {
                authenticate_fails: true,
                ..FakeCssCopier::default()
            },
            FakeCssCopier {
                mirror_fails: true,
                ..FakeCssCopier::default()
            },
        ] {
            for keep in [
                RipOriginal::EncryptedOriginal,
                RipOriginal::DecryptedOriginal,
            ] {
                let (dir, engine) = engine(encrypted_dvd());
                let (outcome, _) =
                    rip_decrypting(&engine, &dir.path().join("rip"), keep, &copier).await;
                let RipOutcome::Ended(done) = outcome else {
                    panic!("withdrawn")
                };
                assert_eq!(done.outcome, "failed");
                assert_eq!(done.error_code.as_deref(), Some("DECRYPTION_FAILED"));
                let detail = done.error_detail.unwrap();
                assert!(detail.contains("exited 1"), "{detail}");
                assert!(done.outputs.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn a_copy_holding_something_unexpected_is_not_sent() {
        let (dir, engine) = engine(encrypted_dvd());
        let copier = FakeCssCopier {
            extra_file: Some("notes.txt".to_owned()),
            ..FakeCssCopier::default()
        };
        let (outcome, _) = rip_decrypting(
            &engine,
            &dir.path().join("rip"),
            RipOriginal::DecryptedOriginal,
            &copier,
        )
        .await;
        let RipOutcome::Ended(done) = outcome else {
            panic!("withdrawn")
        };
        assert_eq!(done.error_code.as_deref(), Some("DECRYPTION_FAILED"));
        assert!(done.error_detail.unwrap().contains("notes.txt"));
    }

    #[tokio::test]
    async fn a_rip_withdrawn_while_decrypting_reports_nothing() {
        let (dir, engine) = engine(encrypted_dvd());
        let copier = FakeCssCopier {
            cancel_during_mirror: true,
            ..FakeCssCopier::default()
        };
        let (outcome, _) = rip_decrypting(
            &engine,
            &dir.path().join("rip"),
            RipOriginal::EncryptedOriginal,
            &copier,
        )
        .await;
        assert_eq!(outcome, RipOutcome::Withdrawn);
    }

    #[tokio::test]
    async fn an_encrypted_dvd_not_asked_to_be_decrypted_is_refused_saying_how_to_rip_it() {
        let (dir, engine) = engine(encrypted_dvd());
        let outcome = rip_disc(
            &engine,
            &drive(),
            &dir.path().join("rip"),
            RipOptions {
                decrypt: None,
                can_decrypt: true,
                ..RipOptions::default()
            },
            &CancelToken::new(),
            &CollectingSink::new(),
        )
        .await;
        let RipOutcome::Ended(done) = outcome else {
            panic!("withdrawn")
        };
        assert_eq!(done.error_code.as_deref(), Some("ENCRYPTED"));
        assert!(
            done.error_detail
                .unwrap()
                .contains("Ask again with decryption")
        );
    }

    async fn rip_with(engine: &FakeEngine, out: &Path, cd: CdReader<'_>) -> RipOutcome {
        rip_disc(
            engine,
            &drive(),
            out,
            RipOptions {
                cd,
                ..RipOptions::default()
            },
            &CancelToken::new(),
            &CollectingSink::new(),
        )
        .await
    }

    #[tokio::test]
    async fn a_cd_on_a_worker_with_redumper_is_read_offset_corrected() {
        let (dir, engine) = engine(pressed_cd());
        let out = dir.path().join("rip");
        let dumper = FakeCdDumper::default();
        let RipOutcome::Ended(done) = rip_with(&engine, &out, CdReader::Corrected(&dumper)).await
        else {
            panic!("withdrawn")
        };
        assert_eq!(done.outcome, "read", "{done:?}");
        assert_eq!(
            done.outputs,
            ["disc.cue", "disc (Track 1).bin", "disc (Track 2).bin"]
        );
        assert_eq!(done.engine.as_deref(), Some("redumper"));
        assert_eq!(done.engine_version.as_deref(), Some("b768"));
        let cd_read = done.cd_read.unwrap();
        assert_eq!(
            (cd_read.read_offset, cd_read.write_offset),
            (Some(6), Some(-6))
        );
        for name in &done.outputs {
            assert!(out.join(name).is_file(), "{name}");
        }
        assert!(
            tangible_domain::rip::check_output_set(&done.outputs, false, None).is_ok(),
            "what the server takes"
        );
        assert!(!out.join("disc.toc").exists(), "cdrdao did not run");
    }

    #[tokio::test]
    async fn redumper_never_reads_a_dvd() {
        let (dir, engine) = engine(FakeBehaviour {
            medium_profile: Some("DVD-ROM".to_owned()),
            medium_sessions: 1,
            ..FakeBehaviour::default()
        });
        let dumper = FakeCdDumper {
            fails: true,
            ..FakeCdDumper::default()
        };
        for cd in [CdReader::Corrected(&dumper), CdReader::Unavailable] {
            let out = dir.path().join("rip");
            let _ = std::fs::remove_dir_all(&out);
            let RipOutcome::Ended(done) = rip_with(&engine, &out, cd).await else {
                panic!("withdrawn")
            };
            assert_eq!(done.outcome, "read", "{done:?}");
            assert_eq!(done.outputs, ["disc.iso"]);
            assert!(done.cd_read.is_none());
        }
    }

    #[tokio::test]
    async fn a_rip_that_insisted_on_redumper_reads_nothing_without_it() {
        let (dir, engine) = engine(pressed_cd());
        let out = dir.path().join("rip");
        let RipOutcome::Ended(done) = rip_with(&engine, &out, CdReader::Unavailable).await else {
            panic!("withdrawn")
        };
        assert_eq!(done.outcome, "refused");
        assert_eq!(done.error_code.as_deref(), Some("CD_ENGINE_UNAVAILABLE"));
        assert_eq!(done.medium.unwrap().profile, "CD-ROM");
        assert!(!out.exists(), "nothing read");
    }

    #[tokio::test]
    async fn a_redumper_rip_that_fails_or_is_withdrawn_ends_as_such() {
        let (dir, engine) = engine(pressed_cd());
        let failing = FakeCdDumper {
            fails: true,
            ..FakeCdDumper::default()
        };
        let RipOutcome::Ended(done) = rip_with(
            &engine,
            &dir.path().join("a"),
            CdReader::Corrected(&failing),
        )
        .await
        else {
            panic!("withdrawn")
        };
        assert_eq!(done.outcome, "failed");
        assert_eq!(done.error_code.as_deref(), Some("READ_FAILED"));
        assert!(done.error_detail.unwrap().contains("even after retrying"));
        assert!(done.outputs.is_empty() && done.cd_read.is_none());

        let withdrawn = FakeCdDumper {
            cancel_during_dump: true,
            ..FakeCdDumper::default()
        };
        assert_eq!(
            rip_with(
                &engine,
                &dir.path().join("b"),
                CdReader::Corrected(&withdrawn)
            )
            .await,
            RipOutcome::Withdrawn
        );
    }

    #[test]
    fn cds_and_dvds_with_something_on_them_are_ripped() {
        let medium = |profile: &str, blank: bool| MediumInfo {
            profile: profile.to_owned(),
            blank,
            ..MediumInfo::default()
        };
        for profile in ["CD-ROM", "CD-R", "CD-RW", "cd-rom"] {
            assert_eq!(rip_decision(&medium(profile, false)), RipDecision::Cd);
        }
        for profile in [
            "DVD-ROM",
            "DVD-RW sequential recording",
            "DVD+R",
            "DVD-R DL",
        ] {
            assert_eq!(rip_decision(&medium(profile, false)), RipDecision::Image);
        }
        assert!(matches!(
            rip_decision(&medium("CD-R", true)),
            RipDecision::Refuse {
                code: "BLANK_MEDIUM",
                ..
            }
        ));
        for profile in ["BD-ROM", "BD-R", "HD DVD-ROM"] {
            assert!(matches!(
                rip_decision(&medium(profile, false)),
                RipDecision::Refuse {
                    code: "MEDIUM_UNSUPPORTED",
                    ..
                }
            ));
        }
    }
}
