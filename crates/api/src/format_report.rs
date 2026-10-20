// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! An image Tangible did not recognise, or read with doubts, written up as a
//! format support request.
//!
//! The report describes structure, never contents: what detection saw, each
//! component by extension, size and a digest prefix, the first bytes of each
//! binary file, and whether known signatures sit at known offsets. A text
//! file (a descriptor) is never dumped, since its first bytes are usually a
//! file name; only a leading keyword such as `FILE` or `[CloneCD]` is kept.
//! Names from the manifest and every file a descriptor declares are given to
//! the scrubber, so a warning quoting one says `<component-2>` instead.
//!
//! A pure function of what the route read, tested against the form's YAML.

use std::fmt::Write as _;

use tangible_domain::report::ScrubContext;

use crate::report::{BuildIdentity, IssueReport, Writer};

/// The form's file name.
pub const FORMAT_TEMPLATE: &str = "format_support.yml";

/// The form's Format options.
pub const FORMATS: [&str; 6] = ["CCD/IMG/SUB", "MDS/MDF", "CHD", "BDMV", "VIDEO_TS", "Other"];

/// Below this detection confidence, an image counts as read with doubts.
pub const DOUBTFUL_CONFIDENCE: f32 = 0.9;

/// Bytes read from the start of each component: enough for the volume
/// recognition area of a 2048-byte or raw 2352-byte image.
pub const HEAD_BYTES: usize = 64 * 1024;

/// Bytes of a binary component's start shown in hex.
const SHOWN_BYTES: usize = 16;

/// Bytes looked at to decide whether a component is text.
const TEXT_PROBE_BYTES: usize = 512;

/// Most components, evidence lines and warnings listed.
const MAX_LISTED: usize = 30;

/// One component, as the report describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentFacts {
    /// Its ordinal in the manifest.
    pub ordinal: u32,
    /// Its role, such as `primary_image`.
    pub role: String,
    /// Its extension, lowercased, if it has a plain one.
    pub extension: Option<String>,
    /// Its size in bytes.
    pub length_bytes: u64,
    /// The first eight hex digits of its SHA-256.
    pub sha256_prefix: String,
    /// Its first bytes, up to [`HEAD_BYTES`], or `None` if they could not be
    /// read.
    pub head: Option<Vec<u8>>,
}

/// Everything the report is built from.
#[derive(Debug, Clone)]
pub struct FormatFacts {
    /// The server's build.
    pub build: BuildIdentity,
    /// The detected format, such as `unknown`.
    pub format: String,
    /// Detection's confidence, 0 to 1.
    pub confidence: f32,
    /// The validation state, such as `valid_with_warnings`.
    pub validation_state: String,
    /// What the detectors observed.
    pub evidence: Vec<String>,
    /// Validation warnings.
    pub warnings: Vec<String>,
    /// The components, in manifest order.
    pub components: Vec<ComponentFacts>,
    /// Standard directory names among the components' paths, such as
    /// `VIDEO_TS`.
    pub directories: Vec<&'static str>,
}

/// Whether an artifact is worth a format report: not recognised, recognised
/// with doubts, or with warnings.
#[must_use]
pub fn worth_reporting(format: &str, confidence: f32, validation_state: &str) -> bool {
    format == "unknown" || confidence < DOUBTFUL_CONFIDENCE || validation_state != "valid"
}

/// Standard disc directory names a component path passes through.
#[must_use]
pub fn directories_in<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<&'static str> {
    let mut found = Vec::new();
    for path in paths {
        for segment in path.split('/') {
            for known in ["VIDEO_TS", "AUDIO_TS", "BDMV", "CERTIFICATE"] {
                if segment.eq_ignore_ascii_case(known) && !found.contains(&known) {
                    found.push(known);
                }
            }
        }
    }
    found
}

/// A plain extension of a file name, lowercased: letters and digits, at most
/// eight of them.
#[must_use]
pub fn plain_extension(file_name: &str) -> Option<String> {
    let (_, extension) = file_name.rsplit_once('.')?;
    let plain = !extension.is_empty()
        && extension.len() <= 8
        && extension.chars().all(|c| c.is_ascii_alphanumeric());
    plain.then(|| extension.to_ascii_lowercase())
}

/// A known signature, and where it was found.
fn signatures(head: &[u8]) -> Vec<&'static str> {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    let mut found = Vec::new();
    if at(0, b"MComprHD") {
        found.push("CHD header (MComprHD) at offset 0");
    }
    if at(0, b"MEDIA DESCRIPTOR") {
        found.push("MDS header (MEDIA DESCRIPTOR) at offset 0");
    }
    if at(0, b"[CloneCD]") {
        found.push("CloneCD control file ([CloneCD]) at offset 0");
    }
    if at(0, b"RIFF") && at(8, b"WAVE") {
        found.push("RIFF WAVE header at offset 0");
    }
    if at(
        0,
        &[
            0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00,
        ],
    ) {
        found.push("raw CD sector sync pattern at offset 0");
    }
    for (offset, what) in [
        (
            32_769,
            "ISO 9660 identifier (CD001) at sector 16 of 2048-byte sectors",
        ),
        (
            37_649,
            "ISO 9660 identifier (CD001) at sector 16 of raw 2352-byte mode 1 sectors",
        ),
        (
            37_657,
            "ISO 9660 identifier (CD001) at sector 16 of raw 2352-byte mode 2 sectors",
        ),
    ] {
        if at(offset, b"CD001") {
            found.push(what);
        }
    }
    let udf = (16..=20).any(|sector| {
        [b"BEA01", b"NSR02", b"NSR03"]
            .iter()
            .any(|magic| at(sector * 2048 + 1, *magic))
    });
    if udf {
        found.push(
            "UDF volume recognition (BEA01 or NSR0x) in sectors 16 to 20 of 2048-byte sectors",
        );
    }
    found
}

/// How a component starts: hex for binary, a leading keyword for text.
fn describe_head(head: &[u8]) -> String {
    if head.is_empty() {
        return "empty".to_owned();
    }
    let probe = &head[..head.len().min(TEXT_PROBE_BYTES)];
    let text = probe
        .iter()
        .all(|&byte| matches!(byte, b'\t' | b'\n' | b'\r' | 0x20..=0x7e));
    if text {
        // The first word only, and only if it looks like a keyword: the rest
        // of a descriptor's first line is usually somebody's file name.
        let first = probe
            .split(u8::is_ascii_whitespace)
            .find(|word| !word.is_empty())
            .unwrap_or_default();
        // An upper-case command (`FILE`, `CD_DA`) or an INI section header
        // (`[CloneCD]`, `[Disc]`), which is how descriptors begin.
        let command = first
            .iter()
            .all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_');
        let section = first.len() > 2
            && first.first() == Some(&b'[')
            && first.last() == Some(&b']')
            && first[1..first.len() - 1]
                .iter()
                .all(u8::is_ascii_alphanumeric);
        let keyword = !first.is_empty() && first.len() <= 16 && (command || section);
        return if keyword {
            format!(
                "text, starting with the keyword {}",
                String::from_utf8_lossy(first)
            )
        } else {
            "text".to_owned()
        };
    }
    let mut hex = String::new();
    for (index, byte) in head.iter().take(SHOWN_BYTES).enumerate() {
        if index > 0 {
            hex.push(' ');
        }
        let _ = write!(hex, "{byte:02x}");
    }
    format!("binary, first bytes {hex}")
}

/// What the bytes suggest the image is, as one of the form's options, or
/// nothing when they do not say.
#[must_use]
pub fn suggested_format(facts: &FormatFacts) -> &'static str {
    let found: Vec<&str> = facts
        .components
        .iter()
        .filter_map(|component| component.head.as_deref())
        .flat_map(signatures)
        .collect();
    let has = |prefix: &str| found.iter().any(|signature| signature.starts_with(prefix));
    if has("CloneCD") {
        "CCD/IMG/SUB"
    } else if has("MDS") {
        "MDS/MDF"
    } else if has("CHD") {
        "CHD"
    } else if facts.directories.contains(&"BDMV") {
        "BDMV"
    } else if facts.directories.contains(&"VIDEO_TS") {
        "VIDEO_TS"
    } else {
        ""
    }
}

/// The structure, as the form's Structure field holds it.
fn structure(facts: &FormatFacts) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Detected: {} (confidence {:.2}), validation: {}",
        facts.format, facts.confidence, facts.validation_state
    );
    list(&mut out, "Evidence", &facts.evidence);
    list(&mut out, "Warnings", &facts.warnings);
    let _ = writeln!(out, "Components:");
    for component in facts.components.iter().take(MAX_LISTED) {
        let _ = writeln!(
            out,
            "- component {}: {}, {} bytes, sha256 {}…, {}",
            component.ordinal,
            component.extension.as_deref().map_or_else(
                || "no extension".to_owned(),
                |extension| format!(".{extension}")
            ),
            component.length_bytes,
            component.sha256_prefix,
            component.role
        );
        match &component.head {
            Some(head) => {
                let _ = writeln!(out, "  starts: {}", describe_head(head));
                let found = signatures(head);
                if !found.is_empty() {
                    let _ = writeln!(out, "  signatures: {}", found.join("; "));
                }
            }
            None => {
                let _ = writeln!(out, "  starts: could not be read");
            }
        }
    }
    if facts.components.len() > MAX_LISTED {
        let _ = writeln!(
            out,
            "- and {} more component(s)",
            facts.components.len() - MAX_LISTED
        );
    }
    let _ = writeln!(
        out,
        "Directories: {}",
        if facts.directories.is_empty() {
            "none of VIDEO_TS, AUDIO_TS, BDMV, CERTIFICATE".to_owned()
        } else {
            facts.directories.join(", ")
        }
    );
    out
}

fn list(out: &mut String, heading: &str, lines: &[String]) {
    if lines.is_empty() {
        let _ = writeln!(out, "{heading}: none");
        return;
    }
    let _ = writeln!(out, "{heading}:");
    for line in lines.iter().take(MAX_LISTED) {
        let _ = writeln!(out, "- {}", line.replace(['\r', '\n'], " "));
    }
    if lines.len() > MAX_LISTED {
        let _ = writeln!(out, "- and {} more", lines.len() - MAX_LISTED);
    }
}

/// Fill in the format support form from `facts`, scrubbing with `context`.
#[must_use]
pub fn format_report(facts: &FormatFacts, context: ScrubContext) -> IssueReport {
    let mut writer = Writer::new(context);
    writer.plain("version", "Tangible version", &facts.build.describe());
    writer.plain("format", "Format", suggested_format(facts));
    writer.scrubbed("structure", "Structure", &structure(facts));
    writer.plain("notes", "Notes", "");
    let title = if facts.format == "unknown" {
        "Format: an image Tangible did not recognise".to_owned()
    } else {
        format!("Format: a {} image read with doubts", facts.format)
    };
    writer.finish(FORMAT_TEMPLATE, Some(title), Some("structure"))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::report::forms::{assert_fits, format};

    fn build() -> BuildIdentity {
        BuildIdentity::current(
            Some("v0.1.0"),
            Some("76fa5157aea012aae85461e694082d553071a606"),
        )
    }

    fn component(ordinal: u32, extension: &str, head: Vec<u8>) -> ComponentFacts {
        ComponentFacts {
            ordinal,
            role: "primary_image".to_owned(),
            extension: Some(extension.to_owned()),
            length_bytes: head.len() as u64,
            sha256_prefix: "1a2b3c4d".to_owned(),
            head: Some(head),
        }
    }

    fn facts(components: Vec<ComponentFacts>) -> FormatFacts {
        FormatFacts {
            build: build(),
            format: "unknown".to_owned(),
            confidence: 0.0,
            validation_state: "valid".to_owned(),
            evidence: vec!["no CD001 identifier at offset 32768".to_owned()],
            warnings: Vec::new(),
            components,
            directories: Vec::new(),
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

    fn with(offset: usize, magic: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0_u8; HEAD_BYTES];
        bytes[offset..offset + magic.len()].copy_from_slice(magic);
        bytes
    }

    #[test]
    fn an_unrecognised_image_fills_the_form() {
        let mdf = with(37_649, b"CD001");
        let mut sync = mdf.clone();
        sync[..12].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 0]);
        let report = format_report(
            &facts(vec![
                component(1, "mdf", sync),
                component(2, "mds", b"MEDIA DESCRIPTOR\x01\x05".to_vec()),
            ]),
            ScrubContext::new(),
        );
        assert_fits(&report, &format());
        assert_eq!(report.template, "format_support.yml");
        assert_eq!(
            report.title.as_deref(),
            Some("Format: an image Tangible did not recognise")
        );
        assert_eq!(field(&report, "version"), "v0.1.0 (76fa515)");
        assert_eq!(field(&report, "format"), "MDS/MDF");
        let structure = field(&report, "structure");
        assert!(structure.starts_with("Detected: unknown (confidence 0.00), validation: valid\n"));
        assert!(structure.contains("Evidence:\n- no CD001 identifier at offset 32768\n"));
        assert!(structure.contains(
            "- component 1: .mdf, 65536 bytes, sha256 1a2b3c4d…, primary_image\n  \
             starts: binary, first bytes 00 ff ff ff ff ff ff ff ff ff ff 00 00 00 00 00\n  \
             signatures: raw CD sector sync pattern at offset 0; ISO 9660 identifier (CD001) \
             at sector 16 of raw 2352-byte mode 1 sectors\n"
        ));
        assert!(structure.contains("signatures: MDS header (MEDIA DESCRIPTOR) at offset 0"));
        assert_eq!(report.long_field.as_deref(), Some("structure"));
    }

    #[test]
    fn a_descriptor_is_never_dumped() {
        let cue = b"FILE \"My Holiday Disc (Disc 1).bin\" BINARY\n  TRACK 01 MODE1/2352\n";
        let report = format_report(
            &facts(vec![component(1, "cue", cue.to_vec())]),
            ScrubContext::new(),
        );
        let structure = field(&report, "structure");
        assert!(
            structure.contains("starts: text, starting with the keyword FILE\n"),
            "{structure}"
        );
        assert!(!structure.contains("Holiday"), "{structure}");
        // Hex of the same text would be as revealing.
        assert!(!structure.contains("4d 79"), "{structure}");
    }

    #[test]
    fn text_that_does_not_start_with_a_keyword_says_only_text() {
        assert_eq!(describe_head(b"my disc notes\n"), "text");
        assert_eq!(
            describe_head(b"[CloneCD]\nVersion=3\n"),
            "text, starting with the keyword [CloneCD]"
        );
        assert_eq!(describe_head(b""), "empty");
    }

    #[test]
    fn names_in_warnings_are_replaced_by_their_labels() {
        let mut facts = facts(vec![component(1, "cue", b"FILE".to_vec())]);
        facts.warnings =
            vec!["Holiday Disc (Track 2).bin could not be used: no staged file matches".to_owned()];
        let context = ScrubContext::new().name("Holiday Disc (Track 2).bin", "declared file 2");
        let report = format_report(&facts, context);
        let structure = field(&report, "structure");
        assert!(
            structure.contains("- <declared-file-2> could not be used: no staged file matches"),
            "{structure}"
        );
        assert_eq!(report.removed.get("name"), Some(&1));
    }

    #[test]
    fn signatures_are_found_where_they_belong() {
        assert_eq!(
            signatures(&with(32_769, b"CD001")),
            ["ISO 9660 identifier (CD001) at sector 16 of 2048-byte sectors"]
        );
        assert_eq!(
            signatures(&with(37_657, b"CD001")),
            ["ISO 9660 identifier (CD001) at sector 16 of raw 2352-byte mode 2 sectors"]
        );
        assert_eq!(
            signatures(&with(17 * 2048 + 1, b"NSR03")),
            ["UDF volume recognition (BEA01 or NSR0x) in sectors 16 to 20 of 2048-byte sectors"]
        );
        assert_eq!(
            signatures(b"MComprHD\0\0\0\x7c"),
            ["CHD header (MComprHD) at offset 0"]
        );
        assert!(
            signatures(&with(32_770, b"CD001")).is_empty(),
            "one byte off"
        );
        assert!(signatures(b"short").is_empty());
    }

    #[test]
    fn the_suggestion_comes_from_the_bytes_or_the_directories() {
        let chd = facts(vec![component(1, "chd", b"MComprHD".to_vec())]);
        assert_eq!(suggested_format(&chd), "CHD");
        let ccd = facts(vec![component(1, "ccd", b"[CloneCD]\n".to_vec())]);
        assert_eq!(suggested_format(&ccd), "CCD/IMG/SUB");
        let mut dvd = facts(vec![component(1, "ifo", b"DVDVIDEO-VMG".to_vec())]);
        dvd.directories = directories_in(["VIDEO_TS/VIDEO_TS.IFO", "AUDIO_TS/x"]);
        assert_eq!(dvd.directories, ["VIDEO_TS", "AUDIO_TS"]);
        assert_eq!(suggested_format(&dvd), "VIDEO_TS");
        // An extension says nothing on its own.
        let named = facts(vec![component(1, "mds", vec![0; 64])]);
        assert_eq!(suggested_format(&named), "");
        for suggestion in ["CHD", "CCD/IMG/SUB", "VIDEO_TS"] {
            assert!(FORMATS.contains(&suggestion));
        }
    }

    #[test]
    fn a_doubtful_or_warned_image_is_worth_a_report_and_a_clean_one_is_not() {
        assert!(worth_reporting("unknown", 0.0, "valid"));
        assert!(worth_reporting("iso", 0.5, "valid"));
        assert!(worth_reporting("cue_bin", 0.99, "valid_with_warnings"));
        assert!(!worth_reporting("iso", 0.99, "valid"));
    }

    #[test]
    fn extensions_are_kept_only_when_plain() {
        assert_eq!(plain_extension("disc.MDF").as_deref(), Some("mdf"));
        assert_eq!(plain_extension("no-extension"), None);
        assert_eq!(plain_extension("odd.name with space"), None);
        assert_eq!(plain_extension("archive.verylongext"), None);
    }

    #[test]
    fn long_lists_are_cut_short() {
        let mut facts = facts((1..=35).map(|n| component(n, "bin", vec![1, 2])).collect());
        facts.warnings = (0..40).map(|n| format!("warning {n}")).collect();
        let report = format_report(&facts, ScrubContext::new());
        let structure = field(&report, "structure");
        assert!(structure.contains("- and 10 more\n"));
        assert!(structure.contains("- and 5 more component(s)\n"));
        assert!(!structure.contains("component 31:"));
    }

    #[test]
    fn a_doubtful_known_format_says_so_in_the_title() {
        let mut facts = facts(Vec::new());
        facts.format = "cue_bin".to_owned();
        facts.confidence = 0.6;
        let report = format_report(&facts, ScrubContext::new());
        assert_eq!(
            report.title.as_deref(),
            Some("Format: a cue_bin image read with doubts")
        );
    }
}
