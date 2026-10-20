// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ripping the disc in the drive, once an operator has asked for it
//! (ADR 0013).
//!
//! As with an erasure, the request names the drive and only the worker can
//! see what is in it, so the decision is made here: an empty drive, a blank
//! disc and anything but a CD are refused, each as what it was. Reading
//! changes nothing on the disc, so a rip that is withdrawn simply stops.

use std::path::Path;

use crate::client::{ReadErrorsBody, RipCompletion};
use crate::engine::{BurnEngine, EngineError, EventSink};
use crate::fake::CancelToken;
use crate::plan::{DriveRef, MediumInfo, RIP_DATA, RIP_TOC, RipRequest};

/// What to do with the disc found in the drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipDecision {
    /// A CD with something on it: read it.
    Rip,
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
/// Only CDs, for now: the profile is how the drive names the disc, and every
/// CD profile starts `CD` (`CD-ROM`, `CD-R`, `CD-RW`). DVDs and Blu-rays need
/// a reader that is not built yet, and are refused rather than read wrongly.
#[must_use]
pub fn rip_decision(medium: &MediumInfo) -> RipDecision {
    if medium.blank {
        return RipDecision::Refuse {
            code: "BLANK_MEDIUM",
            detail: format!("the {} in the drive is blank", medium.profile),
        };
    }
    if !medium.profile.to_ascii_uppercase().starts_with("CD") {
        return RipDecision::Refuse {
            code: "MEDIUM_UNSUPPORTED",
            detail: format!(
                "the disc in the drive is {}; only CDs can be ripped so far",
                medium.profile
            ),
        };
    }
    RipDecision::Rip
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
        error_code: Some(code.to_owned()),
        error_detail: Some(detail),
    }))
}

/// Look in the drive and read the disc into `output_dir`, if it should be.
///
/// Never fails: every ending is an outcome, because an operator waiting on a
/// rip needs to hear how it ended. On a read, the directory holds
/// [`RIP_TOC`] and [`RIP_DATA`] and the completion names them; the caller
/// sends them before reporting it.
pub async fn rip_disc<E: BurnEngine + ?Sized>(
    engine: &E,
    drive: &DriveRef,
    output_dir: &Path,
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
    if let RipDecision::Refuse { code, detail } = rip_decision(&medium) {
        return ended("refused", Some(medium), code, detail);
    }

    let request = RipRequest {
        drive: drive.clone(),
        output_dir: output_dir.to_path_buf(),
        cancel: cancel.clone(),
    };
    match engine.rip_cd(&request, sink).await {
        Ok(report) => RipOutcome::Ended(Box::new(RipCompletion {
            outcome: "read".to_owned(),
            engine: Some(report.engine),
            engine_version: Some(report.engine_version),
            medium: Some(medium),
            read_errors: Some(ReadErrorsBody {
                unreadable_sectors: report.unreadable_sectors,
                retried_sectors: report.retried_sectors,
            }),
            outputs: vec![RIP_TOC.to_owned(), RIP_DATA.to_owned()],
            error_code: None,
            error_detail: None,
        })),
        Err(EngineError::Cancelled) => RipOutcome::Withdrawn,
        Err(EngineError::NoMedium { .. }) => ended(
            "refused",
            Some(medium),
            "NO_MEDIUM",
            "the disc was taken out".to_owned(),
        ),
        Err(EngineError::Timeout { seconds, .. }) => ended(
            "failed",
            Some(medium),
            "READ_TIMEOUT",
            format!("the disc did not finish reading within {seconds} seconds"),
        ),
        Err(error @ EngineError::DriveUnavailable { .. }) => ended(
            "failed",
            Some(medium),
            "DRIVE_UNAVAILABLE",
            error.to_string(),
        ),
        Err(EngineError::Unsupported { what }) => {
            ended("failed", Some(medium), "READ_FAILED", what)
        }
        Err(error) => ended("failed", Some(medium), "READ_FAILED", error.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::engine::CollectingSink;
    use crate::fake::{FakeBehaviour, FakeEngine};

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
    async fn an_empty_drive_a_blank_disc_and_a_dvd_are_refused() {
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
                    medium_profile: Some("DVD-ROM".to_owned()),
                    medium_sessions: 1,
                    ..FakeBehaviour::default()
                },
                "MEDIUM_UNSUPPORTED",
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
        let outcome = rip_disc(&engine, &drive(), &out, &cancel, &CollectingSink::new()).await;
        stopper.await.unwrap();
        assert_eq!(outcome, RipOutcome::Withdrawn);
        assert!(!out.join("disc.bin").exists());
    }

    #[test]
    fn only_cds_with_something_on_them_are_ripped() {
        let medium = |profile: &str, blank: bool| MediumInfo {
            profile: profile.to_owned(),
            blank,
            ..MediumInfo::default()
        };
        for profile in ["CD-ROM", "CD-R", "CD-RW", "cd-rom"] {
            assert_eq!(rip_decision(&medium(profile, false)), RipDecision::Rip);
        }
        assert!(matches!(
            rip_decision(&medium("CD-R", true)),
            RipDecision::Refuse {
                code: "BLANK_MEDIUM",
                ..
            }
        ));
        for profile in ["DVD-ROM", "BD-ROM", "DVD+RW"] {
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
