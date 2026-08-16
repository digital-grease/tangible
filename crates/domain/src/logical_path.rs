// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Portable paths inside an artifact.
//!
//! This is a trust boundary. Paths arrive from CUE sheets, archive entries,
//! and manifests written by other tools, so absolute paths, parent traversal,
//! device paths, NUL bytes, and anything escaping the staging root are
//! rejected outright.
//!
//! A [`LogicalPath`] is deliberately *not* a host filesystem path: it is
//! relative, slash-separated, normalized, and cannot be constructed in an
//! unsafe form. Code that materializes an artifact joins it onto a root, and
//! the type is what makes that join safe.
//!
//! Validation happens once, at construction. Everything downstream can then
//! treat the value as trustworthy without re-checking, which is the whole
//! point of parsing rather than validating.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

/// Longest accepted path, in bytes.
///
/// Bounded because an attacker-supplied descriptor should not be able to
/// allocate without limit, and no legitimate disc image needs more.
pub const MAX_PATH_BYTES: usize = 1024;

/// Longest accepted single path segment, in bytes.
pub const MAX_SEGMENT_BYTES: usize = 255;

/// Why a path was rejected.
///
/// Each variant names one concrete attack or corruption, so a rejection can
/// be reported to the operator precisely rather than as "invalid path".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LogicalPathError {
    /// The path was empty or contained only separators.
    #[error("path is empty")]
    Empty,

    /// The path exceeded [`MAX_PATH_BYTES`].
    #[error("path is {found} bytes, limit is {MAX_PATH_BYTES}")]
    TooLong {
        /// Length supplied.
        found: usize,
    },

    /// A single segment exceeded [`MAX_SEGMENT_BYTES`].
    #[error("path segment {segment:?} exceeds {MAX_SEGMENT_BYTES} bytes")]
    SegmentTooLong {
        /// The offending segment, truncated for reporting.
        segment: String,
    },

    /// The path began with `/`, or with a Windows drive or UNC prefix.
    #[error("path is absolute")]
    Absolute,

    /// The path contained a `..` segment.
    ///
    /// Rejected outright rather than resolved: resolving would silently accept
    /// `a/../../b`, and a descriptor that needs traversal is not one to trust.
    #[error("path contains a parent-directory traversal")]
    ParentTraversal,

    /// The path contained a NUL byte.
    ///
    /// NUL truncates in every C API underneath the runtime, so `a.iso\0.txt`
    /// can pass a suffix check and then open a different file.
    #[error("path contains a NUL byte")]
    NulByte,

    /// The path contained a control character.
    #[error("path contains a control character")]
    ControlCharacter,

    /// The path used a backslash separator.
    ///
    /// Not silently translated: `a\\b` is one legal filename on Unix and two
    /// segments on Windows, and guessing changes what gets written.
    #[error("path contains a backslash; logical paths are slash-separated")]
    Backslash,

    /// A segment was `.`, which carries no information and would denormalize.
    #[error("path contains a current-directory segment")]
    CurrentDirectory,

    /// A segment was a reserved Windows device name such as `CON` or `NUL`.
    ///
    /// These resolve to devices rather than files on Windows regardless of
    /// extension, so materializing such an artifact could write to a device.
    #[error("path segment {segment:?} is a reserved device name")]
    ReservedDeviceName {
        /// The offending segment.
        segment: String,
    },

    /// A segment ended in a space or dot, which Windows silently strips.
    ///
    /// `evil.exe.` and `evil.exe` are the same file there, so accepting the
    /// former defeats any check performed on the latter.
    #[error("path segment {segment:?} ends with a space or dot")]
    TrailingSpaceOrDot {
        /// The offending segment.
        segment: String,
    },
}

/// Windows reserved device names, compared case-insensitively and ignoring
/// any extension.
const RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A validated, portable, relative path within an artifact.
///
/// Guaranteed on construction: non-empty, relative, slash-separated, free of
/// `.` and `..` segments, free of NUL and control characters, within length
/// limits, and safe to join onto a root directory on any supported host.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct LogicalPath(String);

impl LogicalPath {
    /// Validate and normalize a candidate path.
    ///
    /// Normalization is limited to collapsing repeated slashes and dropping a
    /// trailing one. Nothing that changes meaning is repaired: a path needing
    /// repair is rejected so the operator sees it, because an image is never
    /// fixed up during import.
    ///
    /// # Errors
    ///
    /// Returns the specific [`LogicalPathError`] describing why the path is
    /// unsafe or malformed.
    pub fn parse(candidate: &str) -> Result<Self, LogicalPathError> {
        if candidate.len() > MAX_PATH_BYTES {
            return Err(LogicalPathError::TooLong {
                found: candidate.len(),
            });
        }
        if candidate.contains('\0') {
            return Err(LogicalPathError::NulByte);
        }
        if candidate.contains('\\') {
            return Err(LogicalPathError::Backslash);
        }
        if candidate.chars().any(char::is_control) {
            return Err(LogicalPathError::ControlCharacter);
        }
        if candidate.starts_with('/') {
            return Err(LogicalPathError::Absolute);
        }
        if is_windows_absolute(candidate) {
            return Err(LogicalPathError::Absolute);
        }

        let mut segments = Vec::new();
        for segment in candidate.split('/') {
            // Collapses `a//b` and a trailing `/`. Both are formatting noise
            // rather than a change of meaning.
            if segment.is_empty() {
                continue;
            }
            match segment {
                "." => return Err(LogicalPathError::CurrentDirectory),
                ".." => return Err(LogicalPathError::ParentTraversal),
                _ => {}
            }
            if segment.len() > MAX_SEGMENT_BYTES {
                return Err(LogicalPathError::SegmentTooLong {
                    segment: truncate_for_report(segment),
                });
            }
            if segment.ends_with(' ') || segment.ends_with('.') {
                return Err(LogicalPathError::TrailingSpaceOrDot {
                    segment: segment.to_owned(),
                });
            }
            if is_reserved_device_name(segment) {
                return Err(LogicalPathError::ReservedDeviceName {
                    segment: segment.to_owned(),
                });
            }
            segments.push(segment);
        }

        if segments.is_empty() {
            return Err(LogicalPathError::Empty);
        }

        Ok(Self(segments.join("/")))
    }

    /// The normalized path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path segments, in order. Always at least one.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The final segment: the file name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        // A validated path always has a final segment.
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The lowercase extension, if any, without the dot.
    ///
    /// Detection must never rely on this alone: an extension may raise
    /// confidence but cannot override contradictory evidence.
    #[must_use]
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() || ext.is_empty() {
            return None;
        }
        Some(ext.to_ascii_lowercase())
    }

    /// Depth in directory levels. A bare file name has depth 1.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.0.split('/').count()
    }
}

fn is_windows_absolute(candidate: &str) -> bool {
    // UNC, e.g. //server/share. A single leading slash is caught separately.
    if candidate.starts_with("//") {
        return true;
    }
    // Drive-relative and drive-absolute, e.g. C: and C:/x. Both are absolute
    // for our purposes: neither is a portable path inside an artifact.
    let mut chars = candidate.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic()
    )
}

fn is_reserved_device_name(segment: &str) -> bool {
    // Windows resolves CON, CON.txt and CON.tar.gz all to the console device,
    // so compare the portion before the first dot.
    let stem = segment.split('.').next().unwrap_or(segment);
    RESERVED_DEVICE_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

fn truncate_for_report(segment: &str) -> String {
    const REPORT_LIMIT: usize = 64;
    if segment.len() <= REPORT_LIMIT {
        return segment.to_owned();
    }
    let mut end = REPORT_LIMIT;
    while !segment.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &segment[..end])
}

impl fmt::Display for LogicalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for LogicalPath {
    type Err = LogicalPathError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for LogicalPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Deserialization is a trust boundary too: a manifest written by
        // another tool gets exactly the same validation as a CUE sheet.
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn accepts_an_ordinary_file_name() {
        let path = LogicalPath::parse("disc.iso").expect("valid");
        assert_eq!(path.as_str(), "disc.iso");
        assert_eq!(path.file_name(), "disc.iso");
        assert_eq!(path.extension().as_deref(), Some("iso"));
        assert_eq!(path.depth(), 1);
    }

    #[test]
    fn accepts_a_nested_path() {
        let path = LogicalPath::parse("VIDEO_TS/VTS_01_1.VOB").expect("valid");
        assert_eq!(path.depth(), 2);
        assert_eq!(path.file_name(), "VTS_01_1.VOB");
        assert_eq!(path.extension().as_deref(), Some("vob"));
    }

    #[test]
    fn collapses_repeated_and_trailing_slashes() {
        // Formatting noise, not a change of meaning, so normalize rather than
        // reject.
        assert_eq!(
            LogicalPath::parse("a//b///c/").expect("valid").as_str(),
            "a/b/c"
        );
    }

    // --- the unsafe-path rejections -----------------------------------------

    #[test]
    fn rejects_absolute_unix_paths() {
        assert_eq!(
            LogicalPath::parse("/etc/passwd"),
            Err(LogicalPathError::Absolute)
        );
    }

    #[test]
    fn rejects_windows_drive_and_unc_paths() {
        for candidate in ["C:/Windows", "c:", "//server/share/x"] {
            assert_eq!(
                LogicalPath::parse(candidate),
                Err(LogicalPathError::Absolute),
                "{candidate} must be rejected as absolute"
            );
        }
    }

    #[test]
    fn rejects_parent_traversal_anywhere_in_the_path() {
        for candidate in ["../secret", "a/../../secret", "a/b/..", ".."] {
            assert_eq!(
                LogicalPath::parse(candidate),
                Err(LogicalPathError::ParentTraversal),
                "{candidate} must be rejected"
            );
        }
    }

    #[test]
    fn does_not_resolve_traversal_that_stays_within_the_root() {
        // `a/../b` resolves to `b` and is therefore "safe", but a descriptor
        // that needs resolution is not one to trust, and resolving here would
        // mean the same logic has to be correct in every consumer.
        assert_eq!(
            LogicalPath::parse("a/../b"),
            Err(LogicalPathError::ParentTraversal)
        );
    }

    #[test]
    fn rejects_nul_bytes() {
        // NUL truncates in the C APIs underneath: "a.iso\0.txt" would pass an
        // extension check and then open "a.iso".
        assert_eq!(
            LogicalPath::parse("a.iso\0.txt"),
            Err(LogicalPathError::NulByte)
        );
    }

    #[test]
    fn rejects_control_characters() {
        assert_eq!(
            LogicalPath::parse("a\nb.iso"),
            Err(LogicalPathError::ControlCharacter)
        );
    }

    #[test]
    fn rejects_backslashes_rather_than_translating_them() {
        // Translating would guess: "a\\b" is one filename on Unix, two
        // segments on Windows.
        assert_eq!(
            LogicalPath::parse("a\\b.iso"),
            Err(LogicalPathError::Backslash)
        );
    }

    #[test]
    fn rejects_current_directory_segments() {
        assert_eq!(
            LogicalPath::parse("./a.iso"),
            Err(LogicalPathError::CurrentDirectory)
        );
    }

    #[test]
    fn rejects_windows_reserved_device_names_with_any_extension() {
        for candidate in ["CON", "con.txt", "NUL.iso", "sub/LPT1.bin", "AuX.tar.gz"] {
            let error = LogicalPath::parse(candidate).expect_err("must reject");
            assert!(
                matches!(error, LogicalPathError::ReservedDeviceName { .. }),
                "{candidate} produced {error:?}"
            );
        }
    }

    #[test]
    fn allows_names_that_merely_start_with_a_device_name() {
        // CONTENTS is not CON. An over-broad check would reject real files.
        assert!(LogicalPath::parse("CONTENTS.txt").is_ok());
        assert!(LogicalPath::parse("NULLABLE.bin").is_ok());
    }

    #[test]
    fn rejects_trailing_space_or_dot() {
        // Windows strips these silently, making "evil.exe." and "evil.exe"
        // the same file and defeating any check done on the latter.
        for candidate in ["evil.exe.", "evil.exe ", "dir /file.iso"] {
            let error = LogicalPath::parse(candidate).expect_err("must reject");
            assert!(
                matches!(error, LogicalPathError::TrailingSpaceOrDot { .. }),
                "{candidate} produced {error:?}"
            );
        }
    }

    #[test]
    fn rejects_empty_and_separator_only_paths() {
        for candidate in ["", "/", "///"] {
            let error = LogicalPath::parse(candidate).expect_err("must reject");
            assert!(
                matches!(error, LogicalPathError::Empty | LogicalPathError::Absolute),
                "{candidate} produced {error:?}"
            );
        }
    }

    #[test]
    fn enforces_length_limits() {
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        assert_eq!(
            LogicalPath::parse(&long),
            Err(LogicalPathError::TooLong {
                found: MAX_PATH_BYTES + 1
            })
        );

        let long_segment = format!("dir/{}", "a".repeat(MAX_SEGMENT_BYTES + 1));
        assert!(matches!(
            LogicalPath::parse(&long_segment),
            Err(LogicalPathError::SegmentTooLong { .. })
        ));
    }

    #[test]
    fn segment_too_long_report_does_not_split_a_character() {
        // The reported segment is truncated; doing that by byte index would
        // panic on a multi-byte boundary.
        let segment = "é".repeat(MAX_SEGMENT_BYTES);
        let error = LogicalPath::parse(&segment).expect_err("must reject");
        assert!(matches!(error, LogicalPathError::SegmentTooLong { .. }));
    }

    // --- extension handling -------------------------------------------------

    #[test]
    fn extension_is_lowercased_for_comparison() {
        assert_eq!(
            LogicalPath::parse("DISC.ISO").expect("valid").extension(),
            Some("iso".to_owned())
        );
    }

    #[test]
    fn dotfiles_have_no_extension() {
        assert_eq!(
            LogicalPath::parse(".gitkeep").expect("valid").extension(),
            None
        );
    }

    #[test]
    fn files_without_a_dot_have_no_extension() {
        assert_eq!(
            LogicalPath::parse("README").expect("valid").extension(),
            None
        );
    }

    // --- serde --------------------------------------------------------------

    #[test]
    fn serializes_as_a_bare_string() {
        let path = LogicalPath::parse("a/b.iso").expect("valid");
        assert_eq!(
            serde_json::to_string(&path).expect("serialize"),
            "\"a/b.iso\""
        );
    }

    #[test]
    fn deserialization_applies_the_same_validation() {
        // A manifest written by another tool is exactly as untrusted as a CUE
        // sheet, so the trust boundary must hold on the way in.
        let result: Result<LogicalPath, _> = serde_json::from_str("\"../escape\"");
        assert!(
            result.is_err(),
            "traversal must not survive deserialization"
        );
    }

    #[test]
    fn deserialization_normalizes_like_parse() {
        let path: LogicalPath = serde_json::from_str("\"a//b/\"").expect("valid");
        assert_eq!(path.as_str(), "a/b");
    }
}
