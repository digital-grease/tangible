// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `tangible scrub`: an integrity check from the command line
//! (docs/plans/storage-integrity.md), for a person at a shell or a job an
//! administrator schedules themselves.
//!
//! It asks for a check as the API does, then reads it in this process and
//! says how far it has got, so it works whether or not the server is
//! running; when the server is already reading the check, this follows it.
//! Interrupted, it gives the check up, and the server or the next run takes
//! it over where it got to. It exits non-zero when anything was found, so
//! whatever ran it notices.

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tangible_api::diagnostics::bytes;
use tangible_api::integrity::{Integrity, IntegrityRunner, IntegrityRunnerSettings};
use tangible_db::integrity::{IntegrityCheckRecord, Requested, get_check, release_checks};
use tangible_db::{Database, DbConfig};
use tangible_domain::IntegrityCheckState;
use tangible_storage::{FilesystemStore, ManifestStore};

use crate::config::{CommonConfig, ScrubConfig};

/// Who a check asked for here was asked for by.
const ACTOR: &str = "command-line";

/// How often the check is looked at while it runs.
const FOLLOW_EVERY: Duration = Duration::from_secs(2);

/// Ask for a check and, unless told not to wait, read it to its end.
pub async fn scrub_command(common: &CommonConfig, scrub: &ScrubConfig) -> Result<()> {
    let database = Database::connect(&DbConfig::new(common.database_url.expose()))
        .await
        .context("connecting to the database")?;
    let pool = database.pool();
    let artifact = scrub.artifact.map(|id| *id.as_uuid());
    let check = match tangible_db::integrity::request_check(pool, artifact, ACTOR)
        .await
        .context("asking for the check")?
    {
        Requested::Queued(check) => {
            say(&format!("integrity check {} queued", check.id))?;
            check
        }
        Requested::AlreadyActive(check) => {
            say(&format!(
                "integrity check {} was already queued or running; following it",
                check.id
            ))?;
            check
        }
        Requested::NoSuchArtifact => {
            anyhow::bail!("the catalogue has no artifact with that id")
        }
    };
    if scrub.no_wait {
        return Ok(());
    }

    let objects = FilesystemStore::open(&scrub.storage_root)
        .await
        .with_context(|| {
            format!(
                "opening the library root at {}",
                scrub.storage_root.display()
            )
        })?;
    let manifests = ManifestStore::open(objects.clone())
        .await
        .context("opening the manifest store")?;
    // A lease of this process's own, so two runs never think they hold the
    // same check.
    let owner = format!("tangible-scrub-{}", uuid::Uuid::now_v7());
    let runner = IntegrityRunner::new(
        database.clone(),
        objects,
        manifests,
        Integrity::new(),
        IntegrityRunnerSettings {
            owner: owner.clone(),
            poll_interval: FOLLOW_EVERY,
            ..IntegrityRunnerSettings::default()
        },
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut task = tokio::spawn(runner.run(async {
        let _ = stopped.await;
    }));

    let followed = tokio::select! {
        followed = follow(&database, &check) => followed,
        () = crate::shutdown_signal() => Err(anyhow::anyhow!(
            "interrupted; the check was given up, and the server or the next `tangible scrub` \
             goes on from where it got to"
        )),
    };

    // Stop the runner, and give up whatever it holds so it stops at its next
    // progress write rather than reading a whole check first. Given up again
    // until it has stopped, in case it took another check meanwhile.
    let _ = stop.send(());
    loop {
        release_checks(pool, &owner)
            .await
            .context("giving up the check")?;
        if tokio::time::timeout(FOLLOW_EVERY, &mut task).await.is_ok() {
            break;
        }
    }

    let done = followed?;
    for line in outcome(&done) {
        say(&line)?;
    }
    match done.state {
        IntegrityCheckState::Complete if done.problem_count == 0 => Ok(()),
        IntegrityCheckState::Complete => {
            anyhow::bail!("the check found {} problem(s)", done.problem_count)
        }
        IntegrityCheckState::Failed => anyhow::bail!("the check failed"),
        _ => anyhow::bail!("the check was withdrawn"),
    }
}

/// Watch a check until it ends, saying how far it has got.
async fn follow(database: &Database, check: &IntegrityCheckRecord) -> Result<IntegrityCheckRecord> {
    let mut said = String::new();
    loop {
        let now = get_check(database.pool(), check.id)
            .await
            .context("reading the check")?
            .context("the check is gone")?;
        if now.state.is_terminal() {
            return Ok(now);
        }
        let line = progress(&now);
        if line != said {
            say(&line)?;
            said = line;
        }
        tokio::time::sleep(FOLLOW_EVERY).await;
    }
}

/// One line of output, written at once.
fn say(line: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

/// How far a check under way has got.
fn progress(check: &IntegrityCheckRecord) -> String {
    let (Some(objects), Some(total)) = (check.objects_total, check.bytes_total) else {
        return "waiting for its turn".to_owned();
    };
    format!(
        "read {} of {} objects ({} of {}), {} problem(s) so far",
        check.objects_checked,
        objects,
        size(check.bytes_checked),
        size(total),
        check.problem_count
    )
}

/// What a check that ended came to, line by line: what it read, then each
/// problem it kept, then what to do about them.
fn outcome(check: &IntegrityCheckRecord) -> Vec<String> {
    let read = format!(
        "{} object(s) read ({})",
        check.objects_checked,
        size(check.bytes_checked)
    );
    match check.state {
        IntegrityCheckState::Complete if check.problem_count == 0 => {
            vec![format!(
                "integrity check {} complete: {read}, nothing wrong",
                check.id
            )]
        }
        IntegrityCheckState::Complete => {
            let mut lines = vec![format!(
                "integrity check {} complete: {read}, {} problem(s):",
                check.id, check.problem_count
            )];
            let kept = check.problems.as_array().map_or(&[][..], Vec::as_slice);
            lines.extend(kept.iter().map(problem_line));
            let unkept = usize::try_from(check.problem_count)
                .unwrap_or(0)
                .saturating_sub(kept.len());
            if unkept > 0 {
                lines.push(format!("  and {unkept} more, counted but not kept"));
            }
            lines.push(
                "Nothing was repaired. Restore each damaged object from a backup \
                 (deploy/README.md, \"Checking stored bytes\"), then check again."
                    .to_owned(),
            );
            lines
        }
        IntegrityCheckState::Failed => vec![format!(
            "integrity check {} failed after {read}: {}; the server log says more",
            check.id,
            check.error_code.as_deref().unwrap_or("no code recorded")
        )],
        _ => vec![format!(
            "integrity check {} was withdrawn after {read}",
            check.id
        )],
    }
}

/// One problem: its kind, the object's digest when it names one, and what
/// was found.
fn problem_line(problem: &Value) -> String {
    let field = |name| problem.get(name).and_then(Value::as_str).unwrap_or("-");
    let detail: String = field("detail")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("  {} {}: {detail}", field("kind"), field("sha256"))
}

fn size(value: i64) -> String {
    bytes(u64::try_from(value).unwrap_or(0))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use serde_json::json;
    use tangible_domain::{IntegrityCheckId, IntegrityScope};
    use time::OffsetDateTime;

    use super::*;

    fn check(state: IntegrityCheckState) -> IntegrityCheckRecord {
        IntegrityCheckRecord {
            id: IntegrityCheckId::generate(),
            scope: IntegrityScope::All,
            artifact_id: None,
            state,
            objects_total: Some(4),
            bytes_total: Some(4096),
            objects_checked: 2,
            bytes_checked: 2048,
            resume_after: None,
            problems: json!([]),
            problem_count: 0,
            error_code: None,
            error_detail: None,
            created_by: ACTOR.to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            started_at: None,
            completed_at: None,
        }
    }

    #[test]
    fn progress_says_how_far_and_what_was_found() {
        let mut running = check(IntegrityCheckState::Running);
        running.problem_count = 1;
        assert_eq!(
            progress(&running),
            "read 2 of 4 objects (2.0 KiB of 4.0 KiB), 1 problem(s) so far"
        );
        running.objects_total = None;
        running.bytes_total = None;
        assert_eq!(progress(&running), "waiting for its turn");
    }

    #[test]
    fn a_clean_check_is_one_line() {
        let done = check(IntegrityCheckState::Complete);
        let lines = outcome(&done);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("2 object(s) read (2.0 KiB), nothing wrong"));
    }

    #[test]
    fn problems_are_listed_with_what_to_do_and_the_uncounted_rest() {
        let mut done = check(IntegrityCheckState::Complete);
        let digest = "ab".repeat(32);
        done.problems = json!([
            { "kind": "corrupt", "sha256": digest, "detail": "reads back as\nsomething else" },
            { "kind": "manifest_unreadable", "sha256": null, "detail": "cannot be read" },
        ]);
        done.problem_count = 5;
        let lines = outcome(&done);
        assert!(lines[0].ends_with("5 problem(s):"), "{lines:?}");
        assert_eq!(
            lines[1],
            format!("  corrupt {digest}: reads back as something else")
        );
        assert_eq!(lines[2], "  manifest_unreadable -: cannot be read");
        assert_eq!(lines[3], "  and 3 more, counted but not kept");
        assert!(lines[4].starts_with("Nothing was repaired."));
    }

    #[test]
    fn a_failed_or_withdrawn_check_says_so() {
        let mut failed = check(IntegrityCheckState::Failed);
        failed.error_code = Some("storage_unavailable".to_owned());
        assert!(outcome(&failed)[0].contains("failed after 2 object(s) read"));
        assert!(outcome(&failed)[0].contains("storage_unavailable"));
        assert!(outcome(&check(IntegrityCheckState::Canceled))[0].contains("withdrawn"));
    }
}
