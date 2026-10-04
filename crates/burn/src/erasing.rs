// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Erasing the disc in the drive, once an operator has asked for it.
//!
//! The request names the drive; what is actually in it is only known here,
//! once the worker looks. So the decision is made here, from the drive's own
//! report: nothing is done to an empty drive, to a disc that cannot be
//! erased, or to one that is already blank, and each of those is reported as
//! what it was rather than as an erase.

use std::time::Instant;

use crate::client::{ErasureCompletion, ErasureMediumBody};
use crate::engine::{BurnEngine, EngineError, EventSink};
use crate::plan::{BlankRequest, DriveRef, MediumInfo};

/// What to do with the disc found in the drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EraseDecision {
    /// A rewritable disc holding data: erase it.
    Erase,
    /// Already blank: nothing to do.
    AlreadyBlank,
    /// A disc that cannot be erased, such as a CD-R.
    NotRewritable {
        /// What the drive called it.
        profile: String,
    },
}

/// Decide from the drive's report.
///
/// Erasable means the profile names RW or RE, the same rule every engine
/// reports by. A write-once disc is never handed to the erase tool at all,
/// even though the tool would refuse it too: refusing here says why in words
/// an operator can act on.
#[must_use]
pub fn erase_decision(medium: &MediumInfo) -> EraseDecision {
    if !medium.erasable {
        return EraseDecision::NotRewritable {
            profile: medium.profile.clone(),
        };
    }
    if medium.blank {
        return EraseDecision::AlreadyBlank;
    }
    EraseDecision::Erase
}

fn medium_body(medium: &MediumInfo) -> ErasureMediumBody {
    ErasureMediumBody {
        profile: medium.profile.clone(),
        blank: medium.blank,
        rewritable: medium.erasable,
        sessions: medium.sessions,
    }
}

fn ended(
    outcome: &str,
    medium: Option<ErasureMediumBody>,
    error: Option<(&str, String)>,
    duration_seconds: Option<u64>,
) -> ErasureCompletion {
    let (error_code, error_detail) = match error {
        Some((code, detail)) => (Some(code.to_owned()), Some(detail)),
        None => (None, None),
    };
    ErasureCompletion {
        outcome: outcome.to_owned(),
        medium,
        error_code,
        error_detail,
        duration_seconds,
    }
}

/// Look in the drive and erase what is there, if it should be.
///
/// Never fails: every ending, including an engine that could not be reached,
/// is an outcome the server records, because an operator waiting on an
/// erasure needs to hear how it ended.
pub async fn erase_disc<E: BurnEngine + ?Sized>(
    engine: &E,
    drive: &DriveRef,
    full: bool,
    sink: &dyn EventSink,
) -> ErasureCompletion {
    let medium = match engine.inspect_medium(drive).await {
        Ok(medium) => medium,
        Err(EngineError::NoMedium { .. }) => {
            return ended(
                "refused",
                None,
                Some(("NO_MEDIUM", "there is no disc in the drive".to_owned())),
                None,
            );
        }
        Err(EngineError::UnsuitableMedium { reason }) => {
            return ended("refused", None, Some(("UNSUITABLE_MEDIUM", reason)), None);
        }
        Err(error) => {
            return ended(
                "failed",
                None,
                Some(("DRIVE_UNAVAILABLE", error.to_string())),
                None,
            );
        }
    };
    let body = Some(medium_body(&medium));

    match erase_decision(&medium) {
        EraseDecision::AlreadyBlank => ended("already_blank", body, None, Some(0)),
        EraseDecision::NotRewritable { profile } => ended(
            "refused",
            body,
            Some((
                "NOT_REWRITABLE",
                format!("the disc in the drive is {profile}, which cannot be erased"),
            )),
            None,
        ),
        EraseDecision::Erase => {
            let started = Instant::now();
            let request = BlankRequest {
                drive: drive.clone(),
                full,
                // Set here and nowhere else: this function runs only for an
                // erasure somebody asked for by name, for this drive.
                confirmed_destructive: true,
            };
            match engine.blank(&request, sink).await {
                Ok(report) if report.erased => {
                    ended("erased", body, None, Some(report.duration_seconds))
                }
                Ok(report) => ended(
                    "failed",
                    body,
                    Some((
                        "ERASE_FAILED",
                        if report.diagnostics.is_empty() {
                            "the drive did not erase the disc".to_owned()
                        } else {
                            report.diagnostics.join("; ")
                        },
                    )),
                    Some(report.duration_seconds),
                ),
                Err(error) => ended(
                    "failed",
                    body,
                    Some((
                        match error {
                            EngineError::Unsupported { .. } => "ENGINE_CANNOT_ERASE",
                            EngineError::DriveUnavailable { .. } => "DRIVE_UNAVAILABLE",
                            _ => "ERASE_FAILED",
                        },
                        error.to_string(),
                    )),
                    Some(started.elapsed().as_secs()),
                ),
            }
        }
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

    fn engine(profile: &str, sessions: u32) -> (tempfile::TempDir, FakeEngine) {
        let dir = tempfile::tempdir().unwrap();
        let engine = FakeEngine::new(dir.path()).with_behaviour(FakeBehaviour {
            medium_profile: Some(profile.to_owned()),
            medium_sessions: sessions,
            ..FakeBehaviour::default()
        });
        (dir, engine)
    }

    #[tokio::test]
    async fn a_used_rewritable_disc_is_erased() {
        let (_dir, engine) = engine("DVD-RW sequential recording", 1);
        let sink = CollectingSink::new();
        let done = erase_disc(&engine, &drive(), false, &sink).await;
        assert_eq!(done.outcome, "erased", "{done:?}");
        assert_eq!(done.medium.as_ref().unwrap().sessions, 1);
        assert!(sink.events().iter().any(|e| e.code == "BLANK_COMPLETED"));
    }

    #[tokio::test]
    async fn a_blank_rewritable_disc_is_left_alone() {
        let (_dir, engine) = engine("DVD-RW sequential recording", 0);
        let sink = CollectingSink::new();
        let done = erase_disc(&engine, &drive(), false, &sink).await;
        assert_eq!(done.outcome, "already_blank");
        assert!(sink.events().is_empty(), "nothing ran: {:?}", sink.events());
    }

    #[tokio::test]
    async fn a_write_once_disc_is_refused_and_never_handed_to_the_tool() {
        let (_dir, engine) = engine("CD-R", 1);
        let sink = CollectingSink::new();
        let done = erase_disc(&engine, &drive(), true, &sink).await;
        assert_eq!(done.outcome, "refused");
        assert_eq!(done.error_code.as_deref(), Some("NOT_REWRITABLE"));
        assert!(done.error_detail.unwrap().contains("CD-R"));
        assert!(sink.events().is_empty());
    }

    #[tokio::test]
    async fn an_empty_drive_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let engine = FakeEngine::new(dir.path()).with_behaviour(FakeBehaviour {
            no_medium: true,
            ..FakeBehaviour::default()
        });
        let done = erase_disc(&engine, &drive(), false, &CollectingSink::new()).await;
        assert_eq!(done.outcome, "refused");
        assert!(done.medium.is_none());
    }

    #[tokio::test]
    async fn an_unreachable_drive_is_a_failure_not_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let engine = FakeEngine::new(dir.path()).with_behaviour(FakeBehaviour {
            drive_unavailable: true,
            ..FakeBehaviour::default()
        });
        let done = erase_disc(&engine, &drive(), false, &CollectingSink::new()).await;
        assert_eq!(done.outcome, "failed");
        assert_eq!(done.error_code.as_deref(), Some("DRIVE_UNAVAILABLE"));
    }

    #[test]
    fn the_decision_follows_the_profile_and_the_blank_state() {
        let medium = |profile: &str, blank: bool| MediumInfo {
            profile: profile.to_owned(),
            blank,
            writable: true,
            rewritable: profile.contains("RW") || profile.contains("RE"),
            capacity_blocks: 0,
            free_blocks: 0,
            block_size: 2048,
            manufacturer_id: None,
            sessions: u32::from(!blank),
            erasable: profile.contains("RW") || profile.contains("RE"),
            status_warnings: vec![],
        };
        assert_eq!(
            erase_decision(&medium("CD-RW", false)),
            EraseDecision::Erase
        );
        assert_eq!(
            erase_decision(&medium("BD-RE", false)),
            EraseDecision::Erase
        );
        assert_eq!(
            erase_decision(&medium("DVD+RW", true)),
            EraseDecision::AlreadyBlank
        );
        assert_eq!(
            erase_decision(&medium("DVD-R", false)),
            EraseDecision::NotRewritable {
                profile: "DVD-R".to_owned()
            }
        );
    }
}
