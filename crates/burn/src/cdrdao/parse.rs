// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reading what cdrdao said.
//!
//! Like the xorriso parser, everything here reports what the output contains
//! and decides nothing. The fixtures in `fixtures/tool-output/cdrdao/` are why
//! that matters more here than it did there: cdrdao exits 0 from `show-toc` for
//! a table of contents it has just called inconsistent, and exits 1 from a
//! write that it has just said finished successfully. The exit status is
//! context. The lines are the evidence.
//!
//! Two properties of cdrdao's output shape every function below:
//!
//! * **Lines end in `\r` as well as `\n`.** Progress is printed with a
//!   carriage return and no newline, so a whole write's progress is one line
//!   to anything splitting on newlines alone. [`lines`] splits on both.
//! * **Severity is a prefix.** `WARNING:`, `ERROR:`, `INTERNAL ERROR:` and
//!   `FATAL ERROR:` come from one function in cdrdao's source, and everything
//!   else is informational.
//!
//! Unknown lines are ignored rather than refused, because a newer cdrdao adds
//! messages and an old parser that rejected them would refuse a working tool.

use serde::{Deserialize, Serialize};

/// Split output into lines the way cdrdao means them.
///
/// Both `\r` and `\n` end a line, and the empty strings between a `\r\n` or a
/// blank line are dropped.
pub fn lines(output: &str) -> impl Iterator<Item = &str> {
    output
        .split(['\r', '\n'])
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
}

/// How serious cdrdao said a message was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// `WARNING:`. Does not stop anything by itself, with one exception that
    /// matters: a warning from the table-of-contents check stops a write.
    Warning,
    /// `ERROR:`.
    Error,
    /// `INTERNAL ERROR:`, a fault in cdrdao itself.
    InternalError,
    /// `FATAL ERROR:`, after which cdrdao exits.
    Fatal,
}

impl Severity {
    /// Whether this severity means the operation did not do what was asked.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        !matches!(self, Self::Warning)
    }
}

/// One message cdrdao flagged with a severity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// How serious it said it was.
    pub severity: Severity,
    /// What it said, without the prefix.
    pub text: String,
}

/// Read a severity prefix from one line.
///
/// Longest prefix first: `INTERNAL ERROR:` and `FATAL ERROR:` both end in
/// `ERROR:`, and reading them as plain errors would lose which they were.
#[must_use]
pub fn message(line: &str) -> Option<Message> {
    let line = line.trim_start();
    for (prefix, severity) in [
        ("INTERNAL ERROR:", Severity::InternalError),
        ("FATAL ERROR:", Severity::Fatal),
        ("ERROR:", Severity::Error),
        ("WARNING:", Severity::Warning),
    ] {
        if let Some(text) = line.strip_prefix(prefix) {
            return Some(Message {
                severity,
                text: text.trim().to_owned(),
            });
        }
    }
    None
}

/// Every flagged message in some output, in order.
#[must_use]
pub fn messages(output: &str) -> Vec<Message> {
    lines(output).filter_map(message).collect()
}

/// The version cdrdao names in its banner.
///
/// Every command prints `Cdrdao version 1.2.4 - (C) ...` first. `None` means
/// the binary is not cdrdao, rather than that parsing failed.
#[must_use]
pub fn version(output: &str) -> Option<String> {
    lines(output).find_map(|line| {
        let rest = line.strip_prefix("Cdrdao version ")?;
        let version = rest.split_whitespace().next()?;
        Some(version.to_owned())
    })
}

/// Whether the output says the device could not be opened at all.
///
/// Distinct from an empty drive: this is a configuration fault, and the remedy
/// is the device mapping rather than a disc.
#[must_use]
pub fn device_unavailable(output: &str) -> bool {
    messages(output).iter().any(|message| {
        message.severity.is_failure()
            && (message.text.starts_with("Unable to open SCSI device")
                || message.text.starts_with("Cannot setup device"))
    }) && !no_disc(output)
}

/// Whether the output says the drive had no disc in it.
///
/// cdrdao retries for about thirty seconds and then gives up with "Unit not
/// ready". An empty tray is the usual reason; it is not the only one, which
/// is why this is reported as the drive's answer rather than as a certainty.
#[must_use]
pub fn no_disc(output: &str) -> bool {
    messages(output)
        .iter()
        .any(|message| message.text.starts_with("Unit not ready, giving up"))
}

// --- show-toc -------------------------------------------------------------------

/// One track as cdrdao read it from a table of contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedTrack {
    /// Track number, counted by cdrdao from one.
    pub number: u32,
    /// cdrdao's mode name, such as `MODE2_RAW` or `AUDIO`.
    pub mode: String,
    /// The ISRC with cdrdao's spacing removed, when one was declared.
    pub isrc: Option<String>,
    /// Whether digital copying is permitted.
    pub copy_permitted: bool,
    /// Whether pre-emphasis is set. Only reported for audio.
    pub pre_emphasis: bool,
    /// Whether the track is four-channel audio. Only reported for audio.
    pub four_channel: bool,
    /// Pregap in blocks, when the track has one.
    pub pregap: u64,
    /// Where INDEX 01 falls, in blocks from the start of the program area.
    pub start: u64,
    /// Where the track ends, in the same units.
    pub end: u64,
    /// Index points past the first, as absolute block positions.
    pub indexes: Vec<(u32, u64)>,
}

/// What `show-toc` said about a table of contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TocListing {
    /// The disc type line, such as `CD_ROM_XA`.
    pub disc_type: Option<String>,
    /// The catalogue number, when one was declared.
    pub catalog: Option<String>,
    /// Tracks in order.
    pub tracks: Vec<ListedTrack>,
    /// Everything flagged with a severity.
    ///
    /// Any of these means the tool has an objection to the document, and an
    /// objection it prints and then exits 0 from is still an objection.
    pub problems: Vec<Message>,
}

/// Read a `show-toc` listing.
#[must_use]
pub fn toc_listing(output: &str) -> TocListing {
    let mut listing = TocListing::default();

    for line in lines(output) {
        if let Some(message) = message(line) {
            listing.problems.push(message);
            continue;
        }
        let trimmed = line.trim();

        if let Some(value) = trimmed.strip_prefix("TOC TYPE:") {
            listing.disc_type = Some(value.trim().to_owned());
        } else if let Some(value) = trimmed.strip_prefix("CATALOG NUMBER:") {
            listing.catalog = Some(value.trim().to_owned());
        } else if let Some(rest) = trimmed.strip_prefix("TRACK ") {
            // "TRACK  1  Mode MODE2_RAW:" or, with a subchannel,
            // "TRACK  1  Mode MODE1 RW_RAW:"
            let mut words = rest.split_whitespace();
            let number = words.next().and_then(|word| word.parse().ok());
            let mode = match (words.next(), words.next()) {
                (Some("Mode"), Some(mode)) => Some(mode.trim_end_matches(':').to_owned()),
                _ => None,
            };
            if let (Some(number), Some(mode)) = (number, mode) {
                listing.tracks.push(ListedTrack {
                    number,
                    mode,
                    ..ListedTrack::default()
                });
            }
        } else if let Some(track) = listing.tracks.last_mut() {
            track_property(track, trimmed);
        }
    }

    listing
}

/// Apply one indented property line to the track it belongs to.
fn track_property(track: &mut ListedTrack, line: &str) {
    match line {
        "COPY PERMITTED" => track.copy_permitted = true,
        "COPY NOT PERMITTED" => track.copy_permitted = false,
        "PRE-EMPHASIS" => track.pre_emphasis = true,
        "NO PRE-EMPHASIS" => track.pre_emphasis = false,
        "FOUR CHANNEL AUDIO" => track.four_channel = true,
        "TWO CHANNEL AUDIO" => track.four_channel = false,
        _ => {
            if let Some(code) = line.strip_prefix("ISRC ") {
                // Printed as "US RC1 76 07839"; stored without the spaces,
                // which is how the ISRC is written everywhere else.
                track.isrc = Some(code.split_whitespace().collect());
            } else if let Some(rest) = line.strip_prefix("PREGAP ") {
                track.pregap = blocks(rest).unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("START ") {
                track.start = blocks(rest).unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("END") {
                // "END" or "END*" for a padded track.
                track.end = blocks(rest).unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("INDEX ") {
                // "INDEX  2 00:02:00(   150)"
                let number = rest.split_whitespace().next().and_then(|n| n.parse().ok());
                if let (Some(number), Some(position)) = (number, blocks(rest)) {
                    track.indexes.push((number, position));
                }
            }
        }
    }
}

/// Read the block count cdrdao prints in parentheses after a timecode.
///
/// `show-toc` prints only the number, `00:02:00(   150)`; `disk-info` follows
/// it with words, `79:59:74 (359849 blocks, 702/807 MB)`. The first word is
/// the count in both.
fn blocks(text: &str) -> Option<u64> {
    let (_, rest) = text.split_once('(')?;
    let (inside, _) = rest.split_once(')')?;
    inside.split_whitespace().next()?.parse().ok()
}

// --- disk-info and drive-info --------------------------------------------------

/// What `disk-info` said about the medium in a drive.
///
/// Every field is optional because cdrdao prints `n/a` for anything the drive
/// would not answer, and "the drive did not say" is not the same as "no".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskInfo {
    /// Whether the medium is rewritable.
    pub rewritable: Option<bool>,
    /// Total capacity in blocks.
    pub capacity_blocks: Option<u64>,
    /// The medium manufacturer, as cdrdao names it.
    pub manufacturer: Option<String>,
    /// Whether the medium is empty.
    pub empty: Option<bool>,
    /// Sessions already on it, when it is not empty.
    pub sessions: Option<u32>,
    /// Whether another session can be added.
    pub appendable: Option<bool>,
    /// Blocks remaining after the last session, when appendable.
    pub remaining_blocks: Option<u64>,
}

impl DiskInfo {
    /// Whether the report describes a medium at all.
    ///
    /// cdrdao prints the table for any drive it can talk to. Whether it is
    /// empty is the one question that has to be answered for anything else to
    /// mean something.
    #[must_use]
    pub const fn present(&self) -> bool {
        self.empty.is_some()
    }
}

/// Read a `disk-info` report.
#[must_use]
pub fn disk_info(output: &str) -> DiskInfo {
    let mut info = DiskInfo::default();

    for line in output.lines() {
        let Some((label, value)) = line.split_once(" : ").or_else(|| line.split_once(": ")) else {
            continue;
        };
        let label = label.trim();
        let value = value.trim();
        if message(line).is_some() {
            continue;
        }

        match label {
            "CD-RW" => info.rewritable = yes_no(value),
            "Total Capacity" => info.capacity_blocks = blocks(value),
            // A known vendor is followed by a second, indented line naming
            // the dye. It carries no label, so it never reaches here, and it
            // is not wanted: the vendor is what identifies a batch of media.
            "CD-R medium" if value != "n/a" => info.manufacturer = Some(value.to_owned()),
            "CD-R empty" => info.empty = yes_no(value),
            "Sessions" => info.sessions = value.parse().ok(),
            "Appendable" => info.appendable = yes_no(value),
            "Remaining Capacity" => info.remaining_blocks = blocks(value),
            _ => {}
        }
    }
    info
}

/// What `drive-info` said about a drive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveInfo {
    /// The drive's identity from the banner, such as `HL-DT-ST BD-RE WH16NS60`.
    pub identity: Option<String>,
    /// The driver cdrdao chose for it.
    pub driver: Option<String>,
    /// Maximum write speed in kB/s.
    pub max_write_kbps: Option<u32>,
    /// Whether buffer-underrun protection is available.
    pub burn_proof: Option<bool>,
}

/// Read a `drive-info` report, or the device banner from any command.
#[must_use]
pub fn drive_info(output: &str, device: &str) -> DriveInfo {
    let mut info = DriveInfo::default();
    let banner = format!("{device}: ");

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(&banner) {
            // "/dev/sr0: HL-DT-ST BD-RE  WH16NS60\tRev: 1.02"
            let identity = rest.split('\t').next().unwrap_or(rest);
            info.identity = Some(identity.split_whitespace().collect::<Vec<_>>().join(" "));
        } else if let Some(rest) = line.strip_prefix("Using driver: ") {
            info.driver = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("Maximum writing speed:") {
            info.max_write_kbps = rest.split_whitespace().next().and_then(|n| n.parse().ok());
        } else if let Some(rest) = line.strip_prefix("BurnProof supported:") {
            info.burn_proof = yes_no(rest.trim());
        }
    }

    info
}

fn yes_no(value: &str) -> Option<bool> {
    match value {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

// --- write ----------------------------------------------------------------------

/// One thing cdrdao said while writing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteEvent {
    /// A track began.
    TrackStarted {
        /// Its number.
        track: u32,
    },
    /// Bytes are going to the medium.
    Progress {
        /// Megabytes written so far, as cdrdao counts them (2^20 bytes).
        written_mb: u64,
        /// Megabytes in total.
        total_mb: u64,
    },
    /// All data was handed to the drive.
    DataWritten {
        /// Blocks written.
        blocks: u64,
    },
    /// The write finished, according to cdrdao. Its own account, not a
    /// verdict: nothing has read the disc.
    Completed,
    /// Something flagged with a severity.
    Problem(Message),
}

/// What a whole write run said.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteOutcome {
    /// Every event, in order.
    pub events: Vec<WriteEvent>,
    /// Whether the output contains the line that says the write finished.
    pub reported_complete: bool,
    /// Failures before the write was said to be complete: the data did not
    /// land, or did not all land.
    pub failures: Vec<String>,
    /// Failures after it was said to be complete. Trouble finishing up, not a
    /// failed write, and not a success either until the disc is read.
    pub late_problems: Vec<String>,
    /// Warnings, wherever they appeared.
    pub warnings: Vec<String>,
    /// The last progress reading, for reporting how far a failed write got.
    pub last_progress: Option<(u64, u64)>,
    /// Blocks cdrdao said it wrote, when it got that far.
    pub blocks_written: Option<u64>,
}

/// Read one line of write output into an event.
#[must_use]
pub fn write_event(line: &str) -> Option<WriteEvent> {
    if let Some(message) = message(line) {
        return Some(WriteEvent::Problem(message));
    }
    let line = line.trim();
    if line == "Writing finished successfully." || line == "Simulation finished successfully." {
        return Some(WriteEvent::Completed);
    }
    if let Some(rest) = line.strip_prefix("Writing track ") {
        // "Writing track 02 (mode AUDIO/AUDIO )..."
        let track = rest.split_whitespace().next()?.parse().ok()?;
        return Some(WriteEvent::TrackStarted { track });
    }
    if let Some(rest) = line.strip_prefix("Wrote ") {
        // "Wrote 5 of 9 MB (Buffers 100%  95%)." or "Wrote 4150 blocks. ..."
        let mut words = rest.split_whitespace();
        let first: u64 = words.next()?.parse().ok()?;
        return match words.next()? {
            "of" => {
                let total = words.next()?.parse().ok()?;
                (words.next()? == "MB").then_some(WriteEvent::Progress {
                    written_mb: first,
                    total_mb: total,
                })
            }
            word if word.starts_with("blocks") => Some(WriteEvent::DataWritten { blocks: first }),
            _ => None,
        };
    }
    None
}

/// Most events a [`WriteOutcome`] keeps.
///
/// A CD write prints a progress line per megabyte, so a full disc is under a
/// thousand. The cap is for a tool that misbehaves; the summary fields keep
/// updating past it, so nothing that decides the outcome is lost.
const MAX_EVENTS: usize = 4096;

impl WriteOutcome {
    /// Take in one line as it arrives, and say what it was.
    ///
    /// Incremental so that a write is judged from every line it printed, not
    /// from whatever part of a bounded log survived: the completion line comes
    /// last, which is exactly the part a truncated log would lose.
    pub fn observe(&mut self, line: &str) -> Option<WriteEvent> {
        let event = write_event(line)?;
        match &event {
            WriteEvent::Progress {
                written_mb,
                total_mb,
            } => self.last_progress = Some((*written_mb, *total_mb)),
            WriteEvent::DataWritten { blocks } => self.blocks_written = Some(*blocks),
            WriteEvent::Completed => self.reported_complete = true,
            WriteEvent::Problem(message) if message.severity.is_failure() => {
                // Where it appeared decides what it means, as with xorriso.
                if self.reported_complete {
                    self.late_problems.push(message.text.clone());
                } else {
                    self.failures.push(message.text.clone());
                }
            }
            WriteEvent::Problem(message) => self.warnings.push(message.text.clone()),
            WriteEvent::TrackStarted { .. } => {}
        }
        if self.events.len() < MAX_EVENTS {
            self.events.push(event.clone());
        }
        Some(event)
    }
}

/// Read the output of a write.
#[must_use]
pub fn write_outcome(output: &str) -> WriteOutcome {
    let mut outcome = WriteOutcome::default();
    for line in lines(output) {
        let _ = outcome.observe(line);
    }
    outcome
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Real output, captured from cdrdao 1.2.4. See the fixtures README.
    fn captured(name: &str) -> String {
        read(name)
    }

    /// Output assembled from cdrdao's source, for what needs a drive.
    fn assembled(name: &str) -> String {
        read(&format!("assembled/{name}"))
    }

    fn read(name: &str) -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/tool-output/cdrdao/"
        );
        std::fs::read_to_string(format!("{path}{name}"))
            .unwrap_or_else(|error| panic!("read {name}: {error}"))
    }

    // --- lines and messages ---------------------------------------------------

    #[test]
    fn a_carriage_return_ends_a_line() {
        // The property the whole write parser rests on: cdrdao ends progress
        // lines with \r and no newline.
        let output = captured("read-test-mixed.txt");
        let progress = lines(&output)
            .filter(|line| line.starts_with("Read ") && line.contains(" of "))
            .count();
        assert!(progress > 10, "each reading is its own line: {progress}");
        assert!(
            lines(&output).any(|line| line == "Read 4150 blocks."),
            "and the summary after them is found too"
        );
    }

    #[test]
    fn a_severity_is_read_from_its_prefix() {
        assert_eq!(
            message("ERROR: Writing failed."),
            Some(Message {
                severity: Severity::Error,
                text: "Writing failed.".to_owned()
            })
        );
        assert_eq!(
            message("INTERNAL ERROR: x").map(|m| m.severity),
            Some(Severity::InternalError),
            "not read as a plain error"
        );
        assert_eq!(
            message("FATAL ERROR: x").map(|m| m.severity),
            Some(Severity::Fatal)
        );
        assert_eq!(
            message("WARNING: x").map(|m| m.severity),
            Some(Severity::Warning)
        );
        assert_eq!(message("Writing track 01 (mode AUDIO/AUDIO )..."), None);
    }

    #[test]
    fn a_warning_is_not_a_failure() {
        assert!(!Severity::Warning.is_failure());
        assert!(Severity::Error.is_failure());
    }

    // --- version ----------------------------------------------------------------

    #[test]
    fn the_banner_names_the_version() {
        assert_eq!(version(&captured("version.txt")).as_deref(), Some("1.2.4"));
    }

    #[test]
    fn something_that_is_not_cdrdao_names_no_version() {
        assert_eq!(version("sh: cdrdao: not found"), None);
        assert_eq!(version(""), None);
    }

    // --- device and drive state -------------------------------------------------

    #[test]
    fn a_device_that_is_not_there_is_a_configuration_fault() {
        for name in [
            "write-no-device.txt",
            "disk-info-no-device.txt",
            "drive-info-no-device.txt",
        ] {
            let output = captured(name);
            assert!(device_unavailable(&output), "{name}");
            assert!(!no_disc(&output), "{name} is not an empty tray");
        }
    }

    #[test]
    fn an_empty_tray_is_not_a_missing_device() {
        // Different remedies: one is a disc, the other is the device mapping.
        // Captured from a real drive with an empty tray.
        let output = captured("disk-info-no-disc.txt");
        assert!(no_disc(&output));
        assert!(!device_unavailable(&output));
    }

    // --- show-toc -----------------------------------------------------------------

    #[test]
    fn a_mixed_mode_listing_is_read_track_by_track() {
        let listing = toc_listing(&captured("show-toc-mixed.txt"));
        assert_eq!(listing.disc_type.as_deref(), Some("CD_ROM_XA"));
        assert!(listing.problems.is_empty(), "{:?}", listing.problems);
        assert_eq!(listing.tracks.len(), 2);

        let data = &listing.tracks[0];
        assert_eq!(data.number, 1);
        assert_eq!(data.mode, "MODE2_RAW");
        assert_eq!((data.pregap, data.start, data.end), (0, 0, 1000));

        let audio = &listing.tracks[1];
        assert_eq!(audio.mode, "AUDIO");
        assert_eq!((audio.pregap, audio.start, audio.end), (150, 1150, 4150));
        assert!(!audio.pre_emphasis);
        assert!(!audio.four_channel);
        assert!(!audio.copy_permitted);
    }

    #[test]
    fn a_gap_the_file_carries_is_listed_as_a_pregap_too() {
        // cdrdao reads FILE / START / FILE as a pregap of the first FILE's
        // length, so the listing looks the same as for a generated gap, and
        // the end tells them apart: it moves only when the gap is generated.
        let listing = toc_listing(&captured("show-toc-mixed-start.txt"));
        let audio = &listing.tracks[1];
        assert_eq!((audio.pregap, audio.start, audio.end), (150, 1150, 4000));
    }

    #[test]
    fn flags_and_codes_are_read_as_cdrdao_understood_them() {
        let listing = toc_listing(&captured("show-toc-flags.txt"));
        let data = &listing.tracks[0];
        assert!(data.copy_permitted);

        let audio = &listing.tracks[1];
        assert!(audio.copy_permitted);
        assert!(audio.pre_emphasis);
        assert!(audio.four_channel);
        assert_eq!(audio.isrc.as_deref(), Some("USRC17607839"));
    }

    #[test]
    fn a_catalogue_number_and_an_index_are_read() {
        let listing = toc_listing(&captured("show-toc-codes.txt"));
        assert_eq!(listing.catalog.as_deref(), Some("1234567890123"));
        assert_eq!(listing.tracks[0].indexes, vec![(2, 150)]);
    }

    #[test]
    fn an_objection_is_found_even_when_the_tool_exits_zero() {
        // show-toc exits 0 for all three. A check that trusted the exit code
        // would pass a table of contents cdrdao refuses to write.
        for (name, needle) in [
            ("show-toc-missing-file.txt", "Cannot open audio file"),
            ("show-toc-too-long.txt", "exceeds length of audio file"),
            ("show-toc-too-short.txt", "shorter than 4 seconds"),
        ] {
            let listing = toc_listing(&captured(name));
            assert!(
                listing
                    .problems
                    .iter()
                    .any(|problem| problem.text.contains(needle)),
                "{name}: {:?}",
                listing.problems
            );
        }
    }

    #[test]
    fn a_syntax_error_leaves_no_tracks_and_says_why() {
        let listing = toc_listing(&captured("show-toc-syntax-error.txt"));
        assert!(listing.tracks.is_empty());
        assert!(!listing.problems.is_empty());
    }

    // --- disk-info ------------------------------------------------------------------

    #[test]
    fn a_blank_cd_r_is_empty_with_its_whole_capacity() {
        // Captured from a real blank CD-R. The manufacturer is followed by a
        // second line naming the dye, which is deliberately not kept.
        let info = disk_info(&captured("disk-info-blank-cdr.txt"));
        assert!(info.present());
        assert_eq!(info.empty, Some(true));
        assert_eq!(info.rewritable, Some(false));
        assert_eq!(info.capacity_blocks, Some(359_846));
        assert_eq!(
            info.manufacturer.as_deref(),
            Some("CMC Magnetics Corporation")
        );
    }

    #[test]
    fn a_closed_cd_rw_is_written_and_not_appendable() {
        let info = disk_info(&assembled("disk-info-closed-cdrw.txt"));
        assert_eq!(info.empty, Some(false));
        assert_eq!(info.rewritable, Some(true));
        assert_eq!(info.sessions, Some(1));
        assert_eq!(info.appendable, Some(false));
        assert_eq!(info.manufacturer, None, "n/a is not a vendor");
    }

    #[test]
    fn an_appendable_disc_reports_what_is_left() {
        let info = disk_info(&assembled("disk-info-appendable-cdr.txt"));
        assert_eq!(info.appendable, Some(true));
        assert_eq!(info.remaining_blocks, Some(348_449));
    }

    #[test]
    fn a_drive_that_would_not_answer_describes_no_medium() {
        assert!(!disk_info(&captured("disk-info-no-disc.txt")).present());
        assert!(!disk_info(&captured("disk-info-no-device.txt")).present());
    }

    // --- drive-info -------------------------------------------------------------------

    #[test]
    fn a_drive_says_what_it_is_and_what_it_can_do() {
        // Captured from a Slimtype DS8A8SH.
        let info = drive_info(&captured("drive-info.txt"), "/dev/sr0");
        assert_eq!(info.identity.as_deref(), Some("Slimtype DVD A DS8A8SH"));
        assert_eq!(
            info.driver.as_deref(),
            Some("Generic SCSI-3/MMC - Version 2.0 (options 0x0000)")
        );
        assert_eq!(info.max_write_kbps, Some(4234));
        assert_eq!(info.burn_proof, Some(true));
    }

    // --- write --------------------------------------------------------------------------

    #[test]
    fn a_successful_write_says_so_and_reports_its_progress() {
        // The first real burn: a CD-R, the engine's own table of contents, a
        // data track and an audio track, read back byte for byte afterwards.
        let outcome = write_outcome(&captured("write-success.txt"));
        assert!(outcome.reported_complete);
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(outcome.last_progress, Some((7, 7)));
        assert_eq!(outcome.blocks_written, Some(3561));
        assert!(
            outcome
                .events
                .contains(&WriteEvent::TrackStarted { track: 2 }),
            "a track that starts after a \\r is still found"
        );
    }

    #[test]
    fn a_real_simulated_write_completes_and_counts_the_generated_gap() {
        // Captured with --simulate against a blank CD-R: the engine's own
        // arguments and table of contents, the laser off. 3561 blocks is the
        // data track, the audio track and the two-second gap between them.
        let output = captured("write-simulate.txt");
        let outcome = write_outcome(&output);
        assert!(outcome.reported_complete, "{:?}", outcome.events);
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(outcome.blocks_written, Some(1161 + 150 + 2250));
        assert_eq!(outcome.last_progress, Some((7, 7)));
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("Cannot lock memory pages")),
            "a worker without the capability sees this: {:?}",
            outcome.warnings
        );
        assert!(
            output.contains('\r'),
            "the capture keeps cdrdao's carriage returns"
        );
    }

    #[test]
    fn an_underrun_is_a_failed_write_that_says_how_far_it_got() {
        let outcome = write_outcome(&assembled("write-buffer-underrun.txt"));
        assert!(!outcome.reported_complete);
        assert!(
            outcome
                .failures
                .iter()
                .any(|failure| failure.contains("buffer under run")),
            "{:?}",
            outcome.failures
        );
        assert_eq!(outcome.last_progress, Some((5, 9)));
        assert_eq!(outcome.blocks_written, None);
    }

    #[test]
    fn trouble_after_the_write_finished_is_not_a_failed_write() {
        // cdrdao releases the medium lock after saying the write finished,
        // and a failure there exits 1. The data landed; the disc still has to
        // be read before anybody believes it either way.
        let outcome = write_outcome(&assembled("write-complete-then-unlock-failed.txt"));
        assert!(outcome.reported_complete);
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(
            outcome.late_problems,
            vec!["Cannot prevent/allow medium removal.".to_owned()]
        );
    }

    #[test]
    fn a_refusal_before_writing_wrote_nothing() {
        for name in ["write-not-empty.txt", "write-too-long.txt"] {
            let outcome = write_outcome(&assembled(name));
            assert!(!outcome.reported_complete, "{name}");
            assert!(!outcome.failures.is_empty(), "{name}");
            assert_eq!(outcome.last_progress, None, "{name}");
        }
    }

    #[test]
    fn a_missing_device_is_a_write_that_never_started() {
        let outcome = write_outcome(&captured("write-no-device.txt"));
        assert!(!outcome.reported_complete);
        assert!(!outcome.failures.is_empty());
        assert_eq!(outcome.last_progress, None);
    }

    #[test]
    fn a_warning_does_not_fail_a_write() {
        // The container the fixtures were captured in cannot lock memory, and
        // a worker without the capability will see the same warning.
        let outcome = write_outcome(
            "WARNING: Cannot lock memory pages: Cannot allocate memory\n\
             Writing finished successfully.\n",
        );
        assert!(outcome.reported_complete);
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.warnings.len(), 1);
    }

    #[test]
    fn unknown_lines_are_ignored_rather_than_refused() {
        let outcome = write_outcome(
            "Something a later cdrdao prints\n\
             Executing power calibration...\n\
             Writing finished successfully.\n",
        );
        assert!(outcome.reported_complete);
        assert_eq!(outcome.events, vec![WriteEvent::Completed]);
    }

    #[test]
    fn a_progress_line_that_is_not_one_is_not_read_as_one() {
        assert_eq!(write_event("Wrote down some notes"), None);
        assert_eq!(write_event("Wrote 5 of 9 GB"), None);
        assert_eq!(
            write_event("Wrote 5 of 9 MB (Buffer  90%)."),
            Some(WriteEvent::Progress {
                written_mb: 5,
                total_mb: 9
            })
        );
    }
}
