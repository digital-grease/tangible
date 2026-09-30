// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reading what xorriso said.
//!
//! Everything here reports what the output contains and nothing else. The
//! verdict (did this produce a good disc?) belongs to the caller, which has
//! the exit status, the plan and the read-back to combine with it.
//!
//! That separation is not fastidiousness. The fixtures include a run where the
//! write completed and the tool then crashed on shutdown with a non-zero exit,
//! and one where it refused before writing anything. A parser that decided
//! success from the exit code would call the first a failure; one that decided
//! from the text alone would need the second's exit code to know nothing was
//! written. Both facts are reported, and the caller is the only thing holding
//! both.
//!
//! The parsers are line-oriented and tolerant of unknown lines, because a
//! newer xorriso adds messages and an old parser that rejected them would
//! refuse to work with a perfectly good tool.

use serde::{Deserialize, Serialize};

/// What xorriso says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// The version, as reported. Text rather than a parsed triple: it is used
    /// for provenance in a manifest, not for comparison.
    pub version: String,
    /// The libburn version in use, when it is reported.
    ///
    /// Recorded separately because it is the library that talks to the drive,
    /// and a support conversation about a drive is about this number.
    pub libburn: Option<String>,
}

/// Read a `-version` report.
///
/// # Errors
///
/// Returns `None` when the output does not name a version at all, which means
/// the binary is not xorriso rather than that parsing failed.
#[must_use]
pub fn version(output: &str) -> Option<Version> {
    let mut version = None;
    let mut libburn = None;

    for line in output.lines() {
        if let Some(value) = field(line, "xorriso version") {
            version = Some(value.to_owned());
        } else if let Some(value) = field(line, "libburn    in use") {
            // The library reports "1.5.6  (min. 1.5.6)"; the first word is the
            // version in use and the rest is the requirement.
            libburn = value.split_whitespace().next().map(ToOwned::to_owned);
        }
    }

    version.map(|version| Version { version, libburn })
}

/// A drive xorriso can see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// The device node, as xorriso addresses it.
    pub path: String,
    /// The vendor, as the drive reports it.
    pub vendor: Option<String>,
    /// The model, as the drive reports it.
    pub model: Option<String>,
}

/// Read a `-devices` listing.
///
/// An empty list is a normal answer: a machine with no optical drive is a
/// fact, and this is the case a burn worker meets when its device mapping is
/// wrong.
#[must_use]
pub fn devices(output: &str) -> Vec<Device> {
    let mut found = Vec::new();

    for line in output.lines() {
        // The listing looks like:
        //   0  -dev '/dev/sr0'  rwrwrw : 'PIONEER ' 'BD-RW   BDR-212M'
        let Some(rest) = line.split_once("-dev '") else {
            continue;
        };
        let Some((path, tail)) = rest.1.split_once('\'') else {
            continue;
        };

        // The identity is two quoted fields after the permissions, when the
        // drive gave any.
        let mut quoted = tail.split('\'').skip(1).step_by(2);
        let vendor = quoted.next().map(|value| value.trim().to_owned());
        let model = quoted.next().map(|value| value.trim().to_owned());

        found.push(Device {
            path: path.to_owned(),
            vendor: vendor.filter(|value| !value.is_empty()),
            model: model.filter(|value| !value.is_empty()),
        });
    }

    found
}

/// What is in a drive, as `-toc` describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediumReport {
    /// The medium as xorriso classifies it, such as `CD-R` or `stdio file`.
    pub profile: Option<String>,
    /// Whether it holds nothing yet.
    pub blank: bool,
    /// Whether more can be written to it.
    pub appendable: bool,
    /// Blocks that can be read.
    pub readable_blocks: Option<u64>,
    /// Blocks that can still be written.
    pub writable_blocks: Option<u64>,
    /// Sessions already present.
    pub sessions: Option<u32>,
    /// Volume identifiers of the sessions found.
    pub volume_ids: Vec<String>,
    /// The drive's own identity, when it reported one.
    pub drive_identity: Option<String>,
}

impl MediumReport {
    /// Whether anything is present at all.
    ///
    /// A drive with no disc reports no medium rather than an empty one, and
    /// the difference decides whether an operator is asked to insert
    /// something.
    #[must_use]
    pub fn present(&self) -> bool {
        self.profile.is_some()
    }
}

/// Read a `-toc` report.
#[must_use]
pub fn medium(output: &str) -> MediumReport {
    let mut report = MediumReport {
        profile: None,
        blank: false,
        appendable: false,
        readable_blocks: None,
        writable_blocks: None,
        sessions: None,
        volume_ids: Vec::new(),
        drive_identity: None,
    };

    let mut absent = false;

    for line in output.lines() {
        if let Some(value) = field(line, "Media current") {
            // "CD-R" or "stdio file, overwriteable": the first clause is the
            // profile and the rest is how it behaves. "is not recognizable" is
            // not a profile, and reading it as one is how an empty drive came
            // to look like a disc: found on the first real drive, which prints
            // exactly that with nothing in the tray.
            let profile = value.split(',').next().unwrap_or(value).trim();
            report.profile = Some(if profile.starts_with("is not recognizable") {
                "unknown".to_owned()
            } else {
                profile.to_owned()
            });
        } else if let Some(value) = field(line, "Media status") {
            report.blank = value.contains("is blank");
            report.appendable = value.contains("is appendable");
            absent |= value.contains("is not present");
        } else if let Some(value) = field(line, "Media blocks") {
            report.readable_blocks = counted(value, "readable");
            report.writable_blocks = counted(value, "writable");
        } else if let Some(value) = field(line, "Media summary") {
            report.sessions = value
                .split(',')
                .next()
                .and_then(|clause| clause.split_whitespace().next())
                .and_then(|count| count.parse().ok());
        } else if let Some(value) = field(line, "Drive type") {
            report.drive_identity = Some(value.trim().to_owned());
        } else if let Some(value) = field(line, "ISO session") {
            // "  1 ,         0 ,      1649s , TESTVOL"
            if let Some(volume) = value.split(',').nth(3) {
                let volume = volume.trim();
                if !volume.is_empty() {
                    report.volume_ids.push(volume.to_owned());
                }
            }
        }
    }

    if absent {
        // The drive said the tray is empty. Whatever "Media current" said
        // alongside that describes nothing.
        report.profile = None;
    }
    report
}

/// One thing xorriso said while working.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WriteEvent {
    /// Bytes are going to the medium.
    Progress {
        /// Megabytes written so far, as reported.
        written_mb: u64,
        /// Megabytes expected in total.
        total_mb: u64,
        /// Write speed, when reported, as a drive-relative multiple.
        speed: Option<String>,
    },
    /// The write finished, according to xorriso.
    ///
    /// Its own account, not a verdict: the disc has not been read back.
    Completed,
    /// Something went wrong, at a severity xorriso names.
    Problem {
        /// The severity word, such as `FAILURE` or `SORRY`.
        severity: String,
        /// What it said.
        message: String,
    },
}

/// What a whole write run said.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteOutcome {
    /// Every event, in the order they were printed.
    pub events: Vec<WriteEvent>,
    /// Whether the output contains the line that says the write finished.
    ///
    /// Deliberately not called "succeeded". It means the tool said it had
    /// written the data, which is a different claim from the disc being
    /// readable, and a different claim again from the process exiting zero.
    pub reported_complete: bool,
    /// Problems at a severity that stops a burn, reported before the write
    /// finished.
    ///
    /// Only these say the data did not land. Separated from what came
    /// afterwards because the difference decides whether a disc is worth
    /// reading back.
    pub failures: Vec<String>,
    /// Problems reported after the write was said to be complete.
    ///
    /// A tool that wrote the data and then fell over on shutdown is not the
    /// same as one that failed to write. It is not a success either: nothing
    /// has read the disc, so these are carried separately and left for the
    /// caller to weigh.
    pub late_problems: Vec<String>,
    /// The last progress reading, for reporting how far a failed write got.
    pub last_progress: Option<(u64, u64)>,
}

/// Severities xorriso uses for things that stop a job.
const FATAL_SEVERITIES: &[&str] = &["FAILURE", "FATAL", "ABORT"];

/// Read the output of a write.
#[must_use]
pub fn write_outcome(output: &str) -> WriteOutcome {
    let mut events = Vec::new();
    let mut reported_complete = false;
    let mut failures = Vec::new();
    let mut late_problems = Vec::new();
    let mut last_progress = None;

    for line in output.lines() {
        if let Some(event) = progress(line) {
            if let WriteEvent::Progress {
                written_mb,
                total_mb,
                ..
            } = &event
            {
                last_progress = Some((*written_mb, *total_mb));
            }
            events.push(event);
            continue;
        }

        // "Writing to 'stdio:/tmp/target.iso' completed successfully."
        if line.contains("completed successfully") {
            reported_complete = true;
            events.push(WriteEvent::Completed);
            continue;
        }

        if let Some((severity, message)) = problem(line) {
            if FATAL_SEVERITIES.contains(&severity.as_str()) {
                // Where it appeared decides what it means. Before the
                // completion line, the data did not land; after it, the tool
                // had trouble finishing up and the disc still has to be read
                // back before anyone believes it either way.
                if reported_complete {
                    late_problems.push(message.clone());
                } else {
                    failures.push(message.clone());
                }
            }
            events.push(WriteEvent::Problem { severity, message });
        }
    }

    WriteOutcome {
        events,
        reported_complete,
        failures,
        late_problems,
        last_progress,
    }
}

/// One region of a `-check_media` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRegion {
    /// First block of the region.
    pub lba: u64,
    /// Blocks in it.
    pub blocks: u64,
    /// xorriso's verdict, such as `+ good`, `- unreadable` or `0 untested`.
    pub quality: String,
}

impl MediaRegion {
    /// Whether xorriso read this region successfully.
    ///
    /// Its own convention: a verdict beginning `+` was read, `-` was not, and
    /// `0` was not tried.
    #[must_use]
    pub fn read_ok(&self) -> bool {
        self.quality.starts_with('+')
    }

    /// Whether xorriso tried to read this region.
    #[must_use]
    pub fn tested(&self) -> bool {
        !self.quality.starts_with('0')
    }

    /// Whether the region overlaps blocks `first` to `last`.
    #[must_use]
    pub const fn overlaps(&self, first: u64, last: u64) -> bool {
        self.lba <= last && self.lba.saturating_add(self.blocks) > first
    }
}

/// Read the regions of a `-check_media` report.
///
/// The shape, from a real drive:
///   `Media region :        600 ,        561 , + good`
#[must_use]
pub fn media_regions(output: &str) -> Vec<MediaRegion> {
    output
        .lines()
        .filter_map(|line| field(line, "Media region"))
        .filter_map(|value| {
            let mut parts = value.splitn(3, ',');
            let lba = parts.next()?.trim().parse().ok()?;
            let blocks = parts.next()?.trim().parse().ok()?;
            let quality = parts.next()?.trim().to_owned();
            Some(MediaRegion {
                lba,
                blocks,
                quality,
            })
        })
        .collect()
}

/// Parse an UPDATE line into progress.
///
/// The shape is:
///   `xorriso : UPDATE :    5 of  115 MB written (fifo  0%) [buf  50%]`
/// with an optional trailing speed, `  59.7x.`
fn progress(line: &str) -> Option<WriteEvent> {
    let rest = line.split(" UPDATE : ").nth(1)?;
    let (written, rest) = rest.trim().split_once(" of ")?;
    let (total, rest) = rest.trim().split_once(' ')?;
    if !rest.contains("written") {
        return None;
    }

    let speed = rest
        .split_whitespace()
        .find(|word| word.ends_with("x.") || word.ends_with('x'))
        .map(|word| word.trim_end_matches('.').to_owned());

    Some(WriteEvent::Progress {
        written_mb: written.trim().parse().ok()?,
        total_mb: total.trim().parse().ok()?,
        speed,
    })
}

/// Parse a severity line into its severity and message.
///
/// The shape is `xorriso : FAILURE : Image size 58927s exceeds free space...`.
fn problem(line: &str) -> Option<(String, String)> {
    // libburn, the library that actually drives the laser, reports under its
    // own name. The first real write printed "libburn : NOTE : WRITE command
    // repetition happened 335 times", and a FAILURE from it would have been
    // read as nothing at all if only xorriso's prefix were recognised.
    let rest = line
        .strip_prefix("xorriso : ")
        .or_else(|| line.strip_prefix("libburn : "))?;
    let (severity, message) = rest.split_once(" : ")?;
    let severity = severity.trim();
    // Only the words xorriso uses as severities. "xorriso : NOTE : ..." is one
    // of these; a line that merely contains a colon is not.
    if !severity
        .chars()
        .all(|character| character.is_ascii_uppercase())
        || severity.is_empty()
    {
        return None;
    }
    Some((severity.to_owned(), message.trim().to_owned()))
}

/// Read `Name : value` from a report line.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let (label, value) = line.split_once(':')?;
    if label.trim() == name.trim() {
        Some(value.trim())
    } else {
        None
    }
}

/// Read "1664 readable , 118388244 writable" style counts.
fn counted(value: &str, label: &str) -> Option<u64> {
    value
        .split(',')
        .find(|clause| clause.contains(label))
        .and_then(|clause| clause.split_whitespace().next())
        .and_then(|count| count.parse().ok())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Real output, captured from xorriso 1.5.6. See the fixtures README.
    fn fixture(name: &str) -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/tool-output/xorriso/"
        );
        std::fs::read_to_string(format!("{path}{name}"))
            .unwrap_or_else(|error| panic!("read {name}: {error}"))
    }

    #[test]
    fn a_version_report_names_the_tool_and_the_library() {
        let version = version(&fixture("version.txt")).expect("a version");
        assert_eq!(version.version, "1.5.6");
        // The library that talks to the drive, which is what a support
        // conversation about a drive is about.
        assert_eq!(version.libburn.as_deref(), Some("1.5.6"));
    }

    #[test]
    fn something_that_is_not_xorriso_reports_no_version() {
        assert!(version("bash: xorriso: command not found").is_none());
        assert!(version("").is_none());
    }

    #[test]
    fn a_machine_with_no_drives_lists_none() {
        // Not a parse failure: a worker whose device mapping is wrong meets
        // exactly this, and it must be reported as an empty machine.
        assert!(devices(&fixture("devices-none.txt")).is_empty());
    }

    #[test]
    fn a_listed_drive_is_read_with_its_identity() {
        // Captured shape from a machine with a drive. The fixtures here have
        // none, so the line is exercised directly.
        let listing = "\
xorriso 1.5.6 : RockRidge filesystem manipulator, libburnia project.

0  -dev '/dev/sr0'  rwrwrw :  'PIONEER ' 'BD-RW   BDR-212M'
1  -dev '/dev/sr1'  rwrw-- :  'HL-DT-ST' 'BD-RE  WH16NS60'
";
        let found = devices(listing);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].path, "/dev/sr0");
        assert_eq!(found[0].vendor.as_deref(), Some("PIONEER"));
        assert_eq!(found[0].model.as_deref(), Some("BD-RW   BDR-212M"));
        assert_eq!(found[1].path, "/dev/sr1");
    }

    #[test]
    fn a_blank_medium_is_read_as_blank() {
        let medium = medium(&fixture("toc-blank.txt"));
        assert!(medium.present());
        assert!(medium.blank);
        assert_eq!(medium.sessions, Some(0));
        assert_eq!(medium.readable_blocks, Some(0));
        assert_eq!(medium.writable_blocks, Some(118_388_086));
        assert!(medium.volume_ids.is_empty());
    }

    #[test]
    fn a_written_medium_reports_its_session_and_volume() {
        let medium = medium(&fixture("toc-written.txt"));
        assert!(!medium.blank);
        assert!(medium.appendable);
        assert_eq!(medium.sessions, Some(1));
        assert_eq!(medium.readable_blocks, Some(1664));
        assert_eq!(medium.volume_ids, vec!["TESTVOL".to_owned()]);
        assert!(
            medium
                .drive_identity
                .as_deref()
                .is_some_and(|identity| identity.contains("vendor")),
            "the drive identity is worth keeping: {:?}",
            medium.drive_identity
        );
    }

    #[test]
    fn a_real_empty_drive_reports_no_medium() {
        // Captured from a Slimtype DS8A8SH with nothing in the tray. It says
        // "Media current: is not recognizable", which the parser used to take
        // for a profile, so an empty drive read as a disc that could not be
        // written rather than as a request to insert one.
        let empty = medium(&fixture("toc-no-disc.txt"));
        assert!(!empty.present(), "{empty:?}");
        assert!(
            empty
                .drive_identity
                .as_deref()
                .is_some_and(|identity| identity.contains("Slimtype")),
            "{empty:?}"
        );
    }

    #[test]
    fn a_disc_the_drive_cannot_read_is_present_and_unknown() {
        // The other half: the same words with a disc in the tray are not an
        // empty drive, and are not a profile either.
        let unreadable = medium(
            "Media current: is not recognizable\n\
             Media status : is written , is closed\n",
        );
        assert!(unreadable.present());
        assert_eq!(unreadable.profile.as_deref(), Some("unknown"));
    }

    #[test]
    fn a_real_drive_is_listed_with_its_identity() {
        let found = devices(&fixture("devices-one.txt"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "/dev/sr0");
        assert_eq!(found[0].vendor.as_deref(), Some("Slimtype"));
        assert_eq!(found[0].model.as_deref(), Some("DVD A DS8A8SH"));
    }

    #[test]
    fn an_empty_drive_reports_no_medium() {
        // A drive with no disc is not a blank disc, and the difference is
        // whether an operator gets asked to put one in.
        let empty = medium("xorriso 1.5.6 : RockRidge filesystem manipulator.\n");
        assert!(!empty.present());
    }

    #[test]
    fn a_completed_write_says_so_in_its_own_words() {
        let outcome = write_outcome(&fixture("write-success.txt"));
        assert!(outcome.reported_complete);
        assert!(outcome.failures.is_empty());
        assert!(outcome.events.contains(&WriteEvent::Completed));
    }

    #[test]
    fn progress_is_read_with_its_speed() {
        let outcome = write_outcome(&fixture("write-success.txt"));
        let progress: Vec<&WriteEvent> = outcome
            .events
            .iter()
            .filter(|event| matches!(event, WriteEvent::Progress { .. }))
            .collect();
        assert!(!progress.is_empty(), "a long write reports progress");

        assert_eq!(
            progress[0],
            &WriteEvent::Progress {
                written_mb: 5,
                total_mb: 115,
                speed: None
            }
        );
        // The later line carries the speed the drive reached.
        assert!(
            progress
                .iter()
                .any(|event| matches!(event, WriteEvent::Progress { speed: Some(speed), .. } if speed == "59.7x")),
            "{progress:?}"
        );
        assert_eq!(outcome.last_progress, Some((92, 115)));
    }

    #[test]
    fn a_refused_write_reports_the_reason_and_wrote_nothing() {
        let outcome = write_outcome(&fixture("write-insufficient-space.txt"));
        assert!(!outcome.reported_complete);
        assert!(
            outcome
                .failures
                .iter()
                .any(|failure| failure.contains("exceeds free space")),
            "{:?}",
            outcome.failures
        );
        assert_eq!(outcome.last_progress, None, "nothing was written");
    }

    #[test]
    fn a_write_that_finished_before_the_tool_crashed_is_not_a_failed_write() {
        // The fixture that decides this module's shape: xorriso wrote the
        // data, then died on shutdown and exited non-zero. Running the real
        // tool produced exactly this, and treating it as a failed write would
        // have recorded a ruined disc for a disc that holds the right bytes.
        //
        // Where the trouble appeared is what separates them: nothing failed
        // before the write completed, and what came afterwards is about
        // shutting down.
        let outcome = write_outcome(&fixture("write-success-then-abort.txt"));
        assert!(
            outcome.reported_complete,
            "the tool said it completed the write"
        );
        assert!(
            outcome.failures.is_empty(),
            "nothing failed before it completed: {:?}",
            outcome.failures
        );
        assert!(
            !outcome.late_problems.is_empty(),
            "and the shutdown trouble is still recorded: {:?}",
            outcome.events
        );
    }

    #[test]
    fn a_failure_before_the_write_finished_is_a_failed_write() {
        let outcome = write_outcome(&fixture("write-insufficient-space.txt"));
        assert!(!outcome.failures.is_empty());
        assert!(outcome.late_problems.is_empty());
    }

    #[test]
    fn unknown_lines_are_ignored_rather_than_refused() {
        // A newer xorriso adds messages. An old parser that rejected them
        // would refuse to work with a perfectly good tool.
        let outcome = write_outcome(
            "xorriso : NOTE : Something new in a later release\n\
             A line with no structure at all\n\
             Writing to 'stdio:/tmp/x.iso' completed successfully.\n",
        );
        assert!(outcome.reported_complete);
        assert!(outcome.failures.is_empty());
    }

    #[test]
    fn a_real_dummy_write_to_a_cd_r_completes() {
        // Captured with -dummy against a blank CD-R in a real drive: the laser
        // was off and the disc stayed blank, and everything else is real.
        let outcome = write_outcome(&fixture("write-dummy-cdr.txt"));
        assert!(outcome.reported_complete, "{:?}", outcome.events);
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(outcome.last_progress, Some((2, 2)));
    }

    #[test]
    fn the_library_that_drives_the_laser_is_heard_too() {
        let outcome = write_outcome(&fixture("write-dummy-cdr.txt"));
        assert!(
            outcome.events.iter().any(|event| matches!(
                event,
                WriteEvent::Problem { severity, message }
                    if severity == "NOTE" && message.contains("WRITE command repetition")
            )),
            "{:?}",
            outcome.events
        );

        let failed = write_outcome("libburn : FAILURE : Some write failure\n");
        assert_eq!(failed.failures, vec!["Some write failure".to_owned()]);
    }

    #[test]
    fn a_real_blank_cd_r_is_read_as_blank_and_writable() {
        let medium = medium(&fixture("toc-blank-cdr.txt"));
        assert!(medium.present());
        assert!(medium.blank);
        assert_eq!(medium.profile.as_deref(), Some("CD-R"));
        assert_eq!(medium.writable_blocks, Some(359_844));
    }

    #[test]
    fn a_check_media_report_is_read_region_by_region() {
        // Captured from a real drive reading blocks 600 to 1160 of a CD-R.
        let regions = media_regions(
            "Media checks :        lba ,       size , quality\n\
             Media region :          0 ,        600 , 0 untested\n\
             Media region :        600 ,        561 , + good\n\
             Media region :       1161 ,       2400 , 0 untested\n",
        );
        assert_eq!(regions.len(), 3, "the header is not a region");
        assert_eq!(
            regions[1],
            MediaRegion {
                lba: 600,
                blocks: 561,
                quality: "+ good".to_owned()
            }
        );
        assert!(regions[1].read_ok() && regions[1].tested());
        assert!(!regions[0].tested());
        assert!(regions[1].overlaps(600, 1160));
        assert!(
            !regions[0].overlaps(600, 1160),
            "ends where the range begins"
        );
        assert!(!regions[2].overlaps(600, 1160), "begins after it ends");
    }

    #[test]
    fn a_region_that_could_not_be_read_says_so() {
        let regions = media_regions("Media region :        100 ,         16 , - unreadable\n");
        assert!(regions[0].tested());
        assert!(!regions[0].read_ok());
    }

    #[test]
    fn a_note_is_not_a_failure() {
        // Severity is the tool's own word, and only some of them stop a burn.
        let outcome = write_outcome("xorriso : NOTE : -blank as_needed: no need for action\n");
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.events.len(), 1);
    }

    #[test]
    fn a_sorry_is_recorded_without_stopping_the_burn() {
        let outcome = write_outcome("xorriso : SORRY : Something the tool tolerated\n");
        assert!(outcome.failures.is_empty(), "SORRY is tolerated");
        assert_eq!(outcome.events.len(), 1, "and still recorded");
    }
}
