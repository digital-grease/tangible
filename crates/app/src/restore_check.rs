// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `tangible restore-check`: after restoring a backup, whether the catalogue
//! and the library agree (STORAGE.md 12). It looks rather than reads, so it
//! takes seconds; `tangible scrub` then reads every byte. It exits non-zero
//! when anything needs the operator.

use std::io::Write as _;

use anyhow::{Context, Result};
use tangible_api::restore_check::{
    Findings, LISTED, RestoreCheckSettings, RestoreReport, restore_check,
};
use tangible_db::{Database, DbConfig};
use tangible_storage::{FilesystemStore, ManifestStore};

use crate::config::{CommonConfig, RestoreCheckConfig};

/// Check, print what was found, and fail when anything needs attention.
pub async fn restore_check_command(
    common: &CommonConfig,
    config: &RestoreCheckConfig,
) -> Result<()> {
    let database = Database::connect(&DbConfig::new(common.database_url.expose()))
        .await
        .context("connecting to the database")?;
    let objects = FilesystemStore::open(&config.storage_root)
        .await
        .with_context(|| {
            format!(
                "opening the library root at {}",
                config.storage_root.display()
            )
        })?;
    let manifests = ManifestStore::open(objects.clone())
        .await
        .context("opening the manifest store")?;
    let settings = RestoreCheckSettings {
        register: config.register_missing,
        list: LISTED,
    };
    let report = restore_check(&database, &objects, &manifests, settings)
        .await
        .context("checking the restore")?;

    let mut out = std::io::stdout().lock();
    if config.json {
        serde_json::to_writer_pretty(&mut out, &report)?;
        writeln!(out)?;
    } else {
        for line in lines(&report, config.register_missing) {
            writeln!(out, "{line}")?;
        }
    }
    out.flush()?;
    if report.needs_attention() {
        anyhow::bail!("the catalogue and the library do not agree; see above");
    }
    Ok(())
}

fn listed<T>(lines: &mut Vec<String>, findings: &Findings<T>, line: impl Fn(&T) -> String) {
    for finding in &findings.listed {
        lines.push(format!("  {}", line(finding)));
    }
    let unnamed = findings.count.saturating_sub(findings.listed.len());
    if unnamed > 0 {
        lines.push(format!("  and {unnamed} more"));
    }
}

/// The report, for a person.
fn lines(report: &RestoreReport, register: bool) -> Vec<String> {
    let mut lines =
        vec!["Restore check: the catalogue against the library, looked at, not read.".to_owned()];
    if report.pending_migrations.is_empty() {
        lines.push("schema: current".to_owned());
    } else {
        lines.push(format!(
            "schema: {} migration(s) not applied ({}); run `tangible migrate` first",
            report.pending_migrations.len(),
            report.pending_migrations.join(", ")
        ));
    }
    lines.push(format!(
        "objects: {} named by the catalogue, {} missing, {} at another size",
        report.objects_checked, report.missing_objects.count, report.wrong_size.count
    ));
    listed(&mut lines, &report.missing_objects, |digest| {
        format!("missing {digest}")
    });
    listed(&mut lines, &report.wrong_size, |wrong| {
        format!(
            "{} is {} bytes, recorded as {}",
            wrong.sha256, wrong.stored, wrong.recorded
        )
    });
    lines.push(format!(
        "manifests: {} artifact(s) catalogued, {} without a manifest",
        report.artifacts_checked, report.missing_manifests.count
    ));
    listed(&mut lines, &report.missing_manifests, |id| {
        format!("artifact {id} has no manifest")
    });
    if report.uncatalogued.count > 0 {
        lines.push(format!(
            "not catalogued: {} artifact(s) in the library the catalogue does not name{}",
            report.uncatalogued.count,
            if register {
                format!(", {} catalogued again now", report.registered)
            } else {
                "; --register-missing catalogues them again from their manifests".to_owned()
            }
        ));
        listed(&mut lines, &report.uncatalogued, |found| {
            format!("artifact {}: {}", found.artifact_id, found.outcome)
        });
    }
    if !report.unfinished.is_empty() {
        lines.push(
            "unfinished: work the catalogue records as under way, perhaps from before the \
             backup; leases lapse and the runners and workers take it up or say why:"
                .to_owned(),
        );
        for (kind, state, count) in &report.unfinished {
            lines.push(format!("  {count} {kind} job(s) {state}"));
        }
    }
    if !report.missing_objects.is_empty() || !report.wrong_size.is_empty() {
        lines.push(
            "Restore the library again from a copy taken after the database dump; then run \
             `tangible scrub`, which reads every byte."
                .to_owned(),
        );
    } else if !report.needs_attention() {
        lines.push(
            "The catalogue and the library agree. `tangible scrub` reads every byte.".to_owned(),
        );
    }
    lines
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use tangible_api::restore_check::Uncatalogued;

    use super::*;

    #[test]
    fn says_everything_agrees_when_it_does() {
        let text = lines(&RestoreReport::default(), false).join("\n");
        assert!(text.contains("schema: current"), "{text}");
        assert!(
            text.contains("The catalogue and the library agree."),
            "{text}"
        );
    }

    #[test]
    fn says_what_is_missing_and_what_to_do() {
        let mut report = RestoreReport {
            objects_checked: 3,
            pending_migrations: vec!["25/migrate dats".into()],
            ..RestoreReport::default()
        };
        report.missing_objects.count = 1;
        report.missing_objects.listed.push("ab".repeat(32));
        report.uncatalogued.count = 1;
        report.uncatalogued.listed.push(Uncatalogued {
            artifact_id: "a1".into(),
            outcome: "found".into(),
        });
        report.unfinished.push(("burn".into(), "writing".into(), 1));
        let text = lines(&report, false).join("\n");
        assert!(text.contains("1 migration(s) not applied"), "{text}");
        assert!(
            text.contains("3 named by the catalogue, 1 missing"),
            "{text}"
        );
        assert!(
            text.contains(&format!("missing {}", "ab".repeat(32))),
            "{text}"
        );
        assert!(text.contains("--register-missing"), "{text}");
        assert!(text.contains("1 burn job(s) writing"), "{text}");
        assert!(text.contains("Restore the library again"), "{text}");
    }
}
