// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A burn attempt, written up as an issue report.
//!
//! Two kinds. A problem report fills in the burn form (`burn_report.yml`)
//! for any attempt. A "worked" report fills in the hardware form
//! (`hardware_report.yml`) for a disc that was written or verified, which is
//! how the list of tested hardware grows.
//!
//! Everything here is a pure function of [`BurnFacts`], so the mapping onto
//! each form's exact dropdown text and the scrubbing are tested without a
//! database. Free text (the log, the drive's answers, the disc's details)
//! goes through the scrubber with the artifact's file names and volume
//! labels as names to replace; generated values (the build, the engine, the
//! format, the outcome) need none.

use std::collections::BTreeMap;

use tangible_burn::MediumInfo;
use tangible_domain::report::{Removed, ScrubContext, scrub};
use tangible_domain::{BurnAttemptState, DriveConnection};
use time::OffsetDateTime;

use crate::report::{IssueReport, IssueReportField};

/// The burn form's file name.
pub const BURN_TEMPLATE: &str = "burn_report.yml";

/// The hardware form's file name.
pub const HARDWARE_TEMPLATE: &str = "hardware_report.yml";

/// The most of the log kept, before scrubbing. The end is kept: a failure
/// is usually last.
const MAX_LOG_BYTES: usize = 48 * 1024;

/// Which report to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurnReportKind {
    /// Something went wrong: the burn form.
    Problem,
    /// The disc burned and checked: the hardware form.
    Worked,
}

/// What the drive said it is, and how it is attached.
#[derive(Debug, Clone, Default)]
pub struct DriveFacts {
    /// Reported vendor.
    pub vendor: Option<String>,
    /// Reported model.
    pub model: Option<String>,
    /// Reported firmware.
    pub firmware: Option<String>,
    /// How it is attached.
    pub connection: DriveConnection,
}

/// One event, as the log shows it.
#[derive(Debug, Clone)]
pub struct EventLine {
    /// Its sequence number.
    pub sequence: i64,
    /// `stage_changed`, `progress` and the like.
    pub event_type: String,
    /// The stage it names.
    pub stage: String,
    /// Its code.
    pub code: String,
    /// Its message, when it carried one.
    pub message: Option<String>,
    /// When the worker recorded it.
    pub at: OffsetDateTime,
}

/// Everything a burn report is written from.
#[derive(Debug, Clone)]
pub struct BurnFacts {
    /// How the server names its build.
    pub build: String,
    /// Which attempt of its job this was.
    pub attempt_number: i32,
    /// Where it ended, or where it is.
    pub state: BurnAttemptState,
    /// The engine the worker ran.
    pub engine: String,
    /// That engine's version.
    pub engine_version: String,
    /// The failure's code, if it failed.
    pub error_code: Option<String>,
    /// The failure's detail, if it gave one.
    pub error_detail: Option<String>,
    /// The read-back comparison, if one ran.
    pub verify_report: Option<serde_json::Value>,
    /// The drive.
    pub drive: DriveFacts,
    /// What preflight last found in the tray.
    pub medium: Option<MediumInfo>,
    /// The artifact's format, as stored (`iso`, `cue_bin`, ...).
    pub format: String,
    /// The attempt's events, in order.
    pub events: Vec<EventLine>,
    /// Names from the library to keep out of the report, each with the label
    /// it becomes, such as a file name and `component 1`.
    pub names: Vec<(String, String)>,
}

/// Why a report could not be written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BurnReportRefusal {
    /// "Worked" for an attempt that did not produce a disc.
    #[error("only a written or verified burn can be reported as working")]
    DidNotWork,
    /// "Worked" for the fake engine, which has no hardware to vouch for.
    #[error("a burn on the fake engine says nothing about real hardware")]
    FakeDrive,
}

/// Write a report of `kind` from `facts`, scrubbing with `context`.
///
/// # Errors
///
/// A [`BurnReportRefusal`] when a "worked" report is asked for a burn that
/// did not work, or that ran on the fake engine.
pub fn burn_report(
    facts: &BurnFacts,
    kind: BurnReportKind,
    context: ScrubContext,
) -> Result<IssueReport, BurnReportRefusal> {
    let mut context = context;
    for (name, label) in &facts.names {
        context = context.name(name.clone(), label);
    }
    let mut writer = Writer::new(context);

    let drive = drive_line(&facts.drive);
    let media = media_line(facts.medium.as_ref());
    match kind {
        BurnReportKind::Problem => {
            let outcome = outcome_option(facts.state, facts.error_code.as_deref());
            writer.plain("version", "Tangible version", &facts.build);
            writer.scrubbed("drive", "Drive", &drive);
            writer.plain(
                "connection",
                "Connection",
                connection_option(facts.drive.connection),
            );
            writer.scrubbed("media", "Media", &media);
            writer.plain("engine", "Burn engine", engine_option(&facts.engine, true));
            writer.plain("format", "Image format", format_option(&facts.format));
            writer.plain("outcome", "What did Tangible record?", outcome);
            let happened = match &facts.error_code {
                Some(code) => format!("Tangible recorded: {} ({code}).", lowercase_first(outcome)),
                None => format!("Tangible recorded: {}.", lowercase_first(outcome)),
            };
            writer.scrubbed("what_happened", "What happened?", &happened);
            writer.scrubbed("worker_log", "Worker log", &log_text(facts));
            writer.scrubbed(
                "medium",
                "What the drive reports",
                &medium_text(facts.medium.as_ref()),
            );
            Ok(writer.finish(
                BURN_TEMPLATE,
                Some(format!("Burn: {}", lowercase_first(outcome))),
                Some("worker_log"),
            ))
        }
        BurnReportKind::Worked => {
            let result = result_option(facts.state, facts.verify_report.as_ref())
                .ok_or(BurnReportRefusal::DidNotWork)?;
            if facts.engine == "fake" {
                return Err(BurnReportRefusal::FakeDrive);
            }
            writer.plain("version", "Tangible version", &facts.build);
            writer.scrubbed("drive", "Drive", &drive);
            writer.plain(
                "connection",
                "Connection",
                connection_option(facts.drive.connection),
            );
            writer.scrubbed("media", "Media", &media);
            writer.plain("engine", "Burn engine", engine_option(&facts.engine, false));
            writer.plain("format", "Image format", format_option(&facts.format));
            writer.plain("result", "What Tangible checked", result);
            writer.scrubbed(
                "medium",
                "What the drive reported about the disc",
                &medium_text(facts.medium.as_ref()),
            );
            writer.plain("notes", "Anything else", "");
            let title = [drive.as_str(), media.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" with ");
            Ok(writer.finish(
                HARDWARE_TEMPLATE,
                (!title.is_empty()).then(|| format!("Worked: {title}")),
                None,
            ))
        }
    }
}

/// Collects fields, scrubbing the ones that need it and keeping count.
struct Writer {
    context: ScrubContext,
    fields: Vec<IssueReportField>,
    removed: BTreeMap<Removed, usize>,
    truncated: bool,
}

impl Writer {
    fn new(context: ScrubContext) -> Self {
        Self {
            context,
            fields: Vec::new(),
            removed: BTreeMap::new(),
            truncated: false,
        }
    }

    fn plain(&mut self, id: &str, label: &str, value: &str) {
        self.fields.push(IssueReportField {
            id: id.to_owned(),
            label: label.to_owned(),
            value: value.to_owned(),
        });
    }

    fn scrubbed(&mut self, id: &str, label: &str, text: &str) {
        let scrubbed = scrub(text, &self.context);
        for (kind, count) in scrubbed.removed {
            *self.removed.entry(kind).or_insert(0) += count;
        }
        self.truncated |= scrubbed.truncated;
        self.plain(id, label, &scrubbed.text);
    }

    fn finish(
        self,
        template: &str,
        title: Option<String>,
        long_field: Option<&str>,
    ) -> IssueReport {
        IssueReport {
            template: template.to_owned(),
            title,
            fields: self.fields,
            long_field: long_field.map(str::to_owned),
            removed: self
                .removed
                .into_iter()
                .map(|(kind, count)| (removed_name(kind).to_owned(), count))
                .collect(),
            truncated: self.truncated,
        }
    }
}

/// A kind of removal as the API names it, matching its serialised form.
fn removed_name(kind: Removed) -> &'static str {
    match kind {
        Removed::Credential => "credential",
        Removed::HeaderValue => "header_value",
        Removed::UrlCredentials => "url_credentials",
        Removed::Path => "path",
        Removed::Name => "name",
        Removed::Serial => "serial",
        Removed::Address => "address",
        Removed::Host => "host",
    }
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

/// "Slimtype DVD A DS8A8SH, firmware KS21", or nothing when the drive said
/// nothing, which leaves the field for the person.
fn drive_line(drive: &DriveFacts) -> String {
    let name = [drive.vendor.as_deref(), drive.model.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    match (&drive.firmware, name.is_empty()) {
        (Some(firmware), false) => format!("{name}, firmware {firmware}"),
        (Some(firmware), true) => format!("firmware {firmware}"),
        (None, _) => name,
    }
}

/// "CD-R, CMC Magnetics Corporation", or nothing before preflight looked.
fn media_line(medium: Option<&MediumInfo>) -> String {
    let Some(medium) = medium else {
        return String::new();
    };
    match &medium.manufacturer_id {
        Some(maker) => format!("{}, {maker}", medium.profile),
        None => medium.profile.clone(),
    }
}

/// The burn and hardware forms' Connection options. Unknown is left blank
/// for the person rather than guessed as Other.
fn connection_option(connection: DriveConnection) -> &'static str {
    match connection {
        DriveConnection::Usb => "USB",
        DriveConnection::Sata => "SATA",
        DriveConnection::Unknown => "",
    }
}

/// The forms' Burn engine options. Only the burn form offers `fake`.
fn engine_option(engine: &str, fake_allowed: bool) -> &'static str {
    match engine {
        "auto" => "auto",
        "xorriso" => "xorriso",
        "cdrdao" => "cdrdao",
        "fake" if fake_allowed => "fake",
        _ => "Not sure",
    }
}

/// The forms' Image format options.
fn format_option(format: &str) -> &'static str {
    match format {
        "iso" => "ISO",
        "cue_bin" => "CUE/BIN",
        "toc_bin" => "TOC/BIN",
        _ => "Other",
    }
}

/// The burn form's "What did Tangible record?" options.
fn outcome_option(state: BurnAttemptState, error_code: Option<&str>) -> &'static str {
    match (state, error_code) {
        (_, Some("ENGINE_DRIVE_UNAVAILABLE")) => "The drive was never recognised",
        (BurnAttemptState::FailedBeforeWrite, Some(code))
            if code.starts_with("PREFLIGHT_") || code.starts_with("MEDIA_") =>
        {
            "Refused before writing (preflight)"
        }
        (BurnAttemptState::WriteFailed, _) => "Write failed",
        (BurnAttemptState::VerificationFailed, _) => "Written, but verification failed",
        (BurnAttemptState::Verified, _) => "Verified, but the disc does not work where I use it",
        _ => "Something else",
    }
}

/// The hardware form's "What Tangible checked" options; `None` for a burn
/// that produced no disc.
fn result_option(
    state: BurnAttemptState,
    verify_report: Option<&serde_json::Value>,
) -> Option<&'static str> {
    match state {
        BurnAttemptState::Verified => {
            let measured_only = verify_report
                .and_then(|report| report.get("tracks"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|tracks| {
                    tracks.iter().any(|track| {
                        track.get("check").and_then(|c| c.as_str()) != Some("byte_compare")
                    })
                });
            Some(if measured_only {
                "Verified in part, audio tracks measured but not compared"
            } else {
                "Verified, every track compared byte for byte"
            })
        }
        BurnAttemptState::Written => Some("Written, not read back"),
        _ => None,
    }
}

/// The worker log: the attempt's outcome, then its events, the end kept if
/// it is long. Progress readings are summarised rather than listed.
fn log_text(facts: &BurnFacts) -> String {
    let mut header = vec![format!(
        "attempt {}: {} ({} {})",
        facts.attempt_number, facts.state, facts.engine, facts.engine_version
    )];
    if let Some(code) = &facts.error_code {
        header.push(format!("error: {code}"));
    }
    if let Some(detail) = &facts.error_detail {
        header.push(format!("detail: {detail}"));
    }

    let progress = facts
        .events
        .iter()
        .filter(|event| event.event_type == "progress")
        .count();
    let mut lines: Vec<String> = facts
        .events
        .iter()
        .filter(|event| event.event_type != "progress")
        .map(|event| {
            let time = event.at.time();
            let mut line = format!(
                "{:>4} {:02}:{:02}:{:02} {} {}",
                event.sequence,
                time.hour(),
                time.minute(),
                time.second(),
                event.stage,
                event.code
            );
            if let Some(message) = event.message.as_deref().filter(|m| !m.is_empty()) {
                line.push_str(": ");
                line.push_str(message);
            }
            line
        })
        .collect();
    if progress > 0 {
        lines.push(format!("({progress} progress readings left out)"));
    }

    let mut text = header.join("\n");
    text.push_str("\n\n");
    let mut kept = Vec::new();
    let mut size = text.len();
    for line in lines.iter().rev() {
        if size + line.len() + 1 > MAX_LOG_BYTES {
            kept.push("[earlier events left out]".to_owned());
            break;
        }
        size += line.len() + 1;
        kept.push(line.clone());
    }
    kept.reverse();
    text.push_str(&kept.join("\n"));
    text.push('\n');
    text
}

/// What the drive reported about the disc, as lines of text.
fn medium_text(medium: Option<&MediumInfo>) -> String {
    let Some(medium) = medium else {
        return String::new();
    };
    let yes = |value: bool| if value { "yes" } else { "no" };
    let megabytes = medium
        .capacity_blocks
        .saturating_mul(u64::from(medium.block_size))
        / 1_000_000;
    let mut lines = vec![
        format!("profile: {}", medium.profile),
        format!(
            "blank: {}, writable: {}, rewritable: {}, erasable: {}",
            yes(medium.blank),
            yes(medium.writable),
            yes(medium.rewritable),
            yes(medium.erasable)
        ),
        format!(
            "capacity: {} blocks of {} bytes (about {megabytes} MB), {} free",
            medium.capacity_blocks, medium.block_size, medium.free_blocks
        ),
        format!("sessions: {}", medium.sessions),
    ];
    if let Some(maker) = &medium.manufacturer_id {
        lines.push(format!("maker: {maker}"));
    }
    for warning in &medium.status_warnings {
        lines.push(format!("warning: {warning}"));
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A form's field ids, and each dropdown's options, read from its YAML
    /// closely enough for these files: `id:` lines, and the `- ` items under
    /// an `options:` line.
    fn template(yaml: &str) -> BTreeMap<String, Option<Vec<String>>> {
        let mut fields = BTreeMap::new();
        let mut current: Option<String> = None;
        let mut in_options = false;
        for line in yaml.lines() {
            let trimmed = line.trim();
            if let Some(id) = trimmed.strip_prefix("id: ") {
                current = Some(id.trim().to_owned());
                fields.insert(id.trim().to_owned(), None);
                in_options = false;
            } else if trimmed == "options:" {
                in_options = true;
                if let Some(id) = &current {
                    fields.insert(id.clone(), Some(Vec::new()));
                }
            } else if in_options && let Some(option) = trimmed.strip_prefix("- ") {
                if let Some(Some(options)) = current.as_ref().and_then(|id| fields.get_mut(id)) {
                    options.push(option.trim().to_owned());
                }
            } else if !trimmed.is_empty() {
                in_options = false;
            }
        }
        fields
    }

    fn burn_form() -> BTreeMap<String, Option<Vec<String>>> {
        template(include_str!(
            "../../../.github/ISSUE_TEMPLATE/burn_report.yml"
        ))
    }

    fn hardware_form() -> BTreeMap<String, Option<Vec<String>>> {
        template(include_str!(
            "../../../.github/ISSUE_TEMPLATE/hardware_report.yml"
        ))
    }

    /// Every field the report fills is on the form, and every dropdown value
    /// is one of that dropdown's options or empty, exactly: GitHub ignores a
    /// prefilled value it does not recognise.
    fn assert_fits(report: &IssueReport, form: &BTreeMap<String, Option<Vec<String>>>) {
        for field in &report.fields {
            let Some(options) = form.get(&field.id) else {
                panic!("{} is not a field of {}", field.id, report.template);
            };
            if let Some(options) = options
                && !field.value.is_empty()
            {
                assert!(
                    options.contains(&field.value),
                    "{:?} is not an option of {} in {}: {options:?}",
                    field.value,
                    field.id,
                    report.template
                );
            }
        }
    }

    fn medium() -> MediumInfo {
        MediumInfo {
            profile: "CD-R".to_owned(),
            blank: true,
            writable: true,
            capacity_blocks: 359_844,
            free_blocks: 359_844,
            block_size: 2048,
            manufacturer_id: Some("CMC Magnetics Corporation".to_owned()),
            ..MediumInfo::default()
        }
    }

    fn event(
        sequence: i64,
        event_type: &str,
        stage: &str,
        code: &str,
        message: Option<&str>,
    ) -> EventLine {
        EventLine {
            sequence,
            event_type: event_type.to_owned(),
            stage: stage.to_owned(),
            code: code.to_owned(),
            message: message.map(str::to_owned),
            at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn facts(state: BurnAttemptState) -> BurnFacts {
        BurnFacts {
            build: "v0.1.0 (76fa515)".to_owned(),
            attempt_number: 1,
            state,
            engine: "auto".to_owned(),
            engine_version: "xorriso 1.5.4".to_owned(),
            error_code: None,
            error_detail: None,
            verify_report: None,
            drive: DriveFacts {
                vendor: Some("Slimtype".to_owned()),
                model: Some("DVD A DS8A8SH".to_owned()),
                firmware: Some("KS21".to_owned()),
                connection: DriveConnection::Usb,
            },
            medium: Some(medium()),
            format: "cue_bin".to_owned(),
            events: vec![
                event(1, "stage_changed", "staging", "STAGING_STARTED", None),
                event(2, "progress", "writing", "WRITING", None),
                event(
                    3,
                    "engine_message",
                    "writing",
                    "ENGINE_MESSAGE",
                    Some(
                        "FAILURE: could not read /var/lib/tangible-worker/stage/Some Game (USA) (Track 1).bin",
                    ),
                ),
            ],
            names: vec![(
                "Some Game (USA) (Track 1).bin".to_owned(),
                "component 1".to_owned(),
            )],
        }
    }

    fn field<'a>(report: &'a IssueReport, id: &str) -> &'a str {
        &report
            .fields
            .iter()
            .find(|field| field.id == id)
            .unwrap_or_else(|| panic!("no field {id}"))
            .value
    }

    #[test]
    fn a_failed_write_fills_in_the_burn_form() {
        let mut facts = facts(BurnAttemptState::WriteFailed);
        facts.error_code = Some("ENGINE_FAILED".to_owned());
        let report = burn_report(&facts, BurnReportKind::Problem, ScrubContext::new()).unwrap();
        assert_eq!(report.template, BURN_TEMPLATE);
        assert_eq!(report.long_field.as_deref(), Some("worker_log"));
        assert_eq!(field(&report, "version"), "v0.1.0 (76fa515)");
        assert_eq!(
            field(&report, "drive"),
            "Slimtype DVD A DS8A8SH, firmware KS21"
        );
        assert_eq!(field(&report, "connection"), "USB");
        assert_eq!(field(&report, "media"), "CD-R, CMC Magnetics Corporation");
        assert_eq!(field(&report, "engine"), "auto");
        assert_eq!(field(&report, "format"), "CUE/BIN");
        assert_eq!(field(&report, "outcome"), "Write failed");
        assert_eq!(
            field(&report, "what_happened"),
            "Tangible recorded: write failed (ENGINE_FAILED)."
        );
        assert!(field(&report, "medium").contains("maker: CMC Magnetics Corporation"));
        assert_eq!(report.title.as_deref(), Some("Burn: write failed"));
        assert_fits(&report, &burn_form());
    }

    #[test]
    fn the_log_keeps_names_and_paths_out_and_says_so() {
        let mut facts = facts(BurnAttemptState::WriteFailed);
        facts.error_detail = Some("could not open /var/lib/tangible-worker/stage/x.bin".to_owned());
        let report = burn_report(&facts, BurnReportKind::Problem, ScrubContext::new()).unwrap();
        let log = field(&report, "worker_log");
        assert!(!log.contains("Some Game"), "{log}");
        assert!(!log.contains("tangible-worker"), "{log}");
        assert!(
            log.contains("ENGINE_MESSAGE: FAILURE: could not read"),
            "{log}"
        );
        assert!(log.contains("(1 progress readings left out)"), "{log}");
        assert!(report.removed.get("path").copied().unwrap_or(0) >= 1);
        assert!(report.removed.get("name").copied().unwrap_or(0) >= 1);
    }

    #[test]
    fn a_long_log_keeps_its_end() {
        let mut facts = facts(BurnAttemptState::WriteFailed);
        facts.events = (0..20_000)
            .map(|i| event(i, "stage_changed", "writing", &format!("STEP_{i}"), None))
            .collect();
        let report = burn_report(&facts, BurnReportKind::Problem, ScrubContext::new()).unwrap();
        let log = field(&report, "worker_log");
        assert!(log.contains("STEP_19999"), "the last event is kept");
        assert!(!log.contains("STEP_0\n"), "the first is not");
        assert!(log.contains("[earlier events left out]"));
        assert!(
            !report.truncated,
            "kept within what the scrubber reads whole"
        );
    }

    #[test]
    fn every_outcome_and_value_is_one_the_burn_form_offers() {
        for state in BurnAttemptState::all() {
            for code in [
                None,
                Some("ENGINE_DRIVE_UNAVAILABLE"),
                Some("PREFLIGHT_NO_MEDIUM"),
                Some("MEDIA_WAIT_EXPIRED"),
                Some("WORKER_ERROR"),
            ] {
                for engine in ["auto", "xorriso", "cdrdao", "fake", "something-new"] {
                    for format in ["iso", "cue_bin", "toc_bin", "chd", "unknown"] {
                        for connection in [
                            DriveConnection::Usb,
                            DriveConnection::Sata,
                            DriveConnection::Unknown,
                        ] {
                            let mut facts = facts(*state);
                            facts.error_code = code.map(str::to_owned);
                            facts.engine = engine.to_owned();
                            facts.format = format.to_owned();
                            facts.drive.connection = connection;
                            let report =
                                burn_report(&facts, BurnReportKind::Problem, ScrubContext::new())
                                    .unwrap();
                            assert_fits(&report, &burn_form());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn outcomes_map_as_the_form_means_them() {
        use BurnAttemptState as S;
        assert_eq!(
            outcome_option(S::FailedBeforeWrite, Some("ENGINE_DRIVE_UNAVAILABLE")),
            "The drive was never recognised"
        );
        assert_eq!(
            outcome_option(S::FailedBeforeWrite, Some("PREFLIGHT_MEDIUM_NOT_BLANK")),
            "Refused before writing (preflight)"
        );
        assert_eq!(
            outcome_option(S::FailedBeforeWrite, Some("MEDIA_WAIT_EXPIRED")),
            "Refused before writing (preflight)"
        );
        assert_eq!(
            outcome_option(S::WriteFailed, Some("ENGINE_FAILED")),
            "Write failed"
        );
        assert_eq!(
            outcome_option(S::VerificationFailed, None),
            "Written, but verification failed"
        );
        assert_eq!(
            outcome_option(S::Verified, None),
            "Verified, but the disc does not work where I use it"
        );
        assert_eq!(
            outcome_option(S::Interrupted, Some("DRIVE_LOCK_LOST")),
            "Something else"
        );
    }

    #[test]
    fn a_verified_burn_fills_in_the_hardware_form() {
        let mut facts = facts(BurnAttemptState::Verified);
        facts.verify_report = Some(serde_json::json!({
            "state": "match",
            "tracks": [{ "check": "byte_compare" }, { "check": "length_and_readable" }],
        }));
        let report = burn_report(&facts, BurnReportKind::Worked, ScrubContext::new()).unwrap();
        assert_eq!(report.template, HARDWARE_TEMPLATE);
        assert_eq!(report.long_field, None);
        assert_eq!(
            field(&report, "result"),
            "Verified in part, audio tracks measured but not compared"
        );
        assert_eq!(
            report.title.as_deref(),
            Some(
                "Worked: Slimtype DVD A DS8A8SH, firmware KS21 with CD-R, CMC Magnetics Corporation"
            )
        );
        assert_fits(&report, &hardware_form());

        facts.verify_report = Some(serde_json::json!({ "state": "match" }));
        let iso = burn_report(&facts, BurnReportKind::Worked, ScrubContext::new()).unwrap();
        assert_eq!(
            field(&iso, "result"),
            "Verified, every track compared byte for byte"
        );

        facts.state = BurnAttemptState::Written;
        let written = burn_report(&facts, BurnReportKind::Worked, ScrubContext::new()).unwrap();
        assert_eq!(field(&written, "result"), "Written, not read back");
    }

    #[test]
    fn every_value_is_one_the_hardware_form_offers() {
        for state in [BurnAttemptState::Verified, BurnAttemptState::Written] {
            for engine in ["auto", "xorriso", "cdrdao", "something-new"] {
                for format in ["iso", "cue_bin", "toc_bin", "unknown"] {
                    let mut facts = facts(state);
                    facts.engine = engine.to_owned();
                    facts.format = format.to_owned();
                    let report =
                        burn_report(&facts, BurnReportKind::Worked, ScrubContext::new()).unwrap();
                    assert_fits(&report, &hardware_form());
                }
            }
        }
    }

    #[test]
    fn a_burn_that_did_not_work_cannot_be_reported_as_working() {
        for state in [
            BurnAttemptState::WriteFailed,
            BurnAttemptState::VerificationFailed,
            BurnAttemptState::Writing,
        ] {
            assert_eq!(
                burn_report(&facts(state), BurnReportKind::Worked, ScrubContext::new()),
                Err(BurnReportRefusal::DidNotWork),
                "{state}"
            );
        }
        let mut fake = facts(BurnAttemptState::Verified);
        fake.engine = "fake".to_owned();
        assert_eq!(
            burn_report(&fake, BurnReportKind::Worked, ScrubContext::new()),
            Err(BurnReportRefusal::FakeDrive)
        );
    }

    #[test]
    fn what_was_not_known_is_left_for_the_person() {
        let mut facts = facts(BurnAttemptState::FailedBeforeWrite);
        facts.drive = DriveFacts::default();
        facts.medium = None;
        let report = burn_report(&facts, BurnReportKind::Problem, ScrubContext::new()).unwrap();
        for id in ["drive", "connection", "media", "medium"] {
            assert_eq!(field(&report, id), "", "{id}");
        }
    }
}
