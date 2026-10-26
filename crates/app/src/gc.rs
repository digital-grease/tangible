// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `tangible gc`: what garbage collection would remove
//! (docs/plans/storage-integrity.md, phase S2). A dry run, and the only
//! kind there is: it reads the store and the catalogue and removes nothing.

use std::io::Write as _;

use anyhow::{Context, Result};
use tangible_api::diagnostics::bytes;
use tangible_api::gc::{GcPlan, GcSettings, Tally, plan};
use tangible_db::{Database, DbConfig};
use tangible_storage::{FilesystemStore, ManifestStore};

use crate::config::{CommonConfig, GcConfig};

/// Plan a collection and print it.
pub async fn gc_command(common: &CommonConfig, gc: &GcConfig) -> Result<()> {
    let database = Database::connect(&DbConfig::new(common.database_url.expose()))
        .await
        .context("connecting to the database")?;
    let objects = FilesystemStore::open(&gc.storage_root)
        .await
        .with_context(|| format!("opening the library root at {}", gc.storage_root.display()))?;
    let manifests = ManifestStore::open(objects)
        .await
        .context("opening the manifest store")?;
    let keep = if gc.limit == 0 { usize::MAX } else { gc.limit };
    let planned = plan(&database, &manifests, GcSettings::days(gc.grace_days, keep))
        .await
        .context("planning the collection")?;

    let mut out = std::io::stdout().lock();
    if gc.json {
        serde_json::to_writer_pretty(&mut out, &planned)?;
        writeln!(out)?;
    } else {
        for line in lines(&planned) {
            writeln!(out, "{line}")?;
        }
    }
    out.flush()?;
    Ok(())
}

/// The plan, for a person.
fn lines(plan: &GcPlan) -> Vec<String> {
    let days = plan.grace_seconds / 86_400;
    let grace = match days {
        0 => "no grace period".to_owned(),
        1 => "the 1-day grace period".to_owned(),
        _ => format!("the {days}-day grace period"),
    };
    let mut lines = vec![
        "Garbage collection plan, a dry run: nothing was removed.".to_owned(),
        format!("stored: {}", objects(plan.stored)),
        format!("live: {}", objects(plan.live)),
    ];
    if plan.recent.count > 0 {
        lines.push(format!(
            "recent: {}, unreferenced and kept for {grace}",
            objects(plan.recent)
        ));
    }
    lines.push(format!(
        "collectable: {}, unreferenced and older than {grace}: what collection would free",
        objects(plan.collectable)
    ));
    for candidate in &plan.candidates {
        lines.push(format!(
            "  {}  {}  written {}{}",
            candidate.sha256,
            bytes(candidate.size_bytes),
            candidate.modified_at,
            if candidate.recorded {
                ""
            } else {
                "  (never catalogued)"
            }
        ));
    }
    let unnamed = plan
        .collectable
        .count
        .saturating_sub(count(&plan.candidates));
    if unnamed > 0 {
        lines.push(format!("  and {unnamed} more (--limit 0 names them all)"));
    }
    if plan.partial_files.count > 0 {
        lines.push(format!(
            "partial files: {} ({}) left by ingests that did not finish, {} older than {grace}",
            plan.partial_files.count,
            bytes(plan.partial_files.bytes),
            plan.stale_partial_files
        ));
    }
    if plan.unrecognized.count > 0 {
        lines.push(format!(
            "not objects: {} entr{} ({}) in the object tree that are neither objects nor partial \
             files; never collected",
            plan.unrecognized.count,
            if plan.unrecognized.count == 1 {
                "y"
            } else {
                "ies"
            },
            bytes(plan.unrecognized.bytes)
        ));
    }
    if plan.missing.count > 0 {
        lines.push(format!(
            "missing: {} the catalogue records have no file; `tangible scrub` says which \
             artifacts they belong to",
            objects(plan.missing)
        ));
        for digest in &plan.missing_digests {
            lines.push(format!("  {digest}"));
        }
        let unnamed = plan
            .missing
            .count
            .saturating_sub(count(&plan.missing_digests));
        if unnamed > 0 {
            lines.push(format!("  and {unnamed} more"));
        }
    }
    if !plan.certain {
        lines.push(format!(
            "Uncertain: {} manifest(s) and {} import checkpoint(s) could not be read, and any of \
             them may name a candidate; collection would remove nothing until they can be. The \
             log says which.",
            plan.manifests_unreadable, plan.checkpoints_unreadable
        ));
    }
    lines
}

fn objects(tally: Tally) -> String {
    let noun = if tally.count == 1 {
        "object"
    } else {
        "objects"
    };
    format!("{} {noun} ({})", tally.count, bytes(tally.bytes))
}

fn count<T>(named: &[T]) -> u64 {
    u64::try_from(named.len()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use tangible_api::gc::GcCandidate;

    use super::*;

    fn plan() -> GcPlan {
        GcPlan {
            planned_at: "2026-10-25T00:00:00Z".to_owned(),
            grace_seconds: 7 * 86_400,
            stored: Tally {
                count: 4,
                bytes: 4096,
            },
            live: Tally {
                count: 3,
                bytes: 3072,
            },
            recent: Tally::default(),
            collectable: Tally {
                count: 1,
                bytes: 1024,
            },
            candidates: Vec::new(),
            partial_files: Tally::default(),
            stale_partial_files: 0,
            unrecognized: Tally::default(),
            missing: Tally::default(),
            missing_digests: Vec::new(),
            manifests_unreadable: 0,
            checkpoints_unreadable: 0,
            certain: true,
        }
    }

    #[test]
    fn a_tidy_store_is_a_few_lines() {
        let mut tidy = plan();
        tidy.candidates.push(GcCandidate {
            sha256: "ab".repeat(32),
            size_bytes: 1024,
            modified_at: "2026-09-01T00:00:00Z".to_owned(),
            recorded: false,
        });
        let lines = lines(&tidy);
        assert_eq!(
            lines,
            vec![
                "Garbage collection plan, a dry run: nothing was removed.".to_owned(),
                "stored: 4 objects (4.0 KiB)".to_owned(),
                "live: 3 objects (3.0 KiB)".to_owned(),
                "collectable: 1 object (1.0 KiB), unreferenced and older than the 7-day grace \
                 period: what collection would free"
                    .to_owned(),
                format!(
                    "  {}  1.0 KiB  written 2026-09-01T00:00:00Z  (never catalogued)",
                    "ab".repeat(32)
                ),
            ]
        );
    }

    #[test]
    fn everything_else_found_is_said_and_doubt_is_said_last() {
        let mut messy = plan();
        messy.grace_seconds = 0;
        messy.recent = Tally { count: 1, bytes: 1 };
        messy.partial_files = Tally {
            count: 2,
            bytes: 10,
        };
        messy.stale_partial_files = 1;
        messy.unrecognized = Tally { count: 1, bytes: 3 };
        messy.missing = Tally {
            count: 3,
            bytes: 2048,
        };
        messy.missing_digests = vec!["cd".repeat(32)];
        messy.manifests_unreadable = 1;
        messy.certain = false;
        let lines = lines(&messy);
        let text = lines.join("\n");
        assert!(
            text.contains("recent: 1 object (1 bytes), unreferenced and kept for no grace period")
        );
        assert!(text.contains("  and 1 more (--limit 0 names them all)"));
        assert!(
            text.contains(
                "partial files: 2 (10 bytes) left by ingests that did not finish, 1 older"
            )
        );
        assert!(text.contains("not objects: 1 entry (3 bytes)"));
        assert!(text.contains("missing: 3 objects (2.0 KiB) the catalogue records have no file"));
        assert!(text.contains(&format!("  {}\n  and 2 more", "cd".repeat(32))));
        assert!(
            lines
                .last()
                .unwrap()
                .starts_with("Uncertain: 1 manifest(s) and 0 import")
        );
    }
}
