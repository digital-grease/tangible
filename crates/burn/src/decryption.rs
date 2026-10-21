// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Whether this worker's image can decrypt a disc (ADR 0013).
//!
//! The image Tangible publishes cannot. An operator who wants it builds
//! `deploy/decryption/Dockerfile`, which installs the libraries and writes a
//! record of what it installed. The worker reports that record as a
//! capability, but only after finding the library and the tool at the fixed
//! places it looks for them itself: nothing here takes a path from the record,
//! the server, or the environment, and no library is loaded by the worker.
//! The tools load them.
//!
//! Only DVD (CSS) decryption is reported for now. The record also names
//! libaacs, which needs the Blu-ray helper that is not built yet.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Where the opt-in image writes its record, relative to the filesystem root.
pub const RECORD: &str = "usr/share/tangible/decryption.json";

/// Where the opt-in image installs libdvdcss.
pub const LIBDVDCSS: &str = "usr/local/lib/libdvdcss.so.2";

/// Where Debian installs dvdbackup.
pub const DVDBACKUP: &str = "usr/bin/dvdbackup";

/// Longest record read. The real one is about 200 bytes.
const MAX_RECORD_BYTES: u64 = 4096;

/// Longest version string accepted.
const MAX_VERSION_CHARS: usize = 64;

/// What this worker can decrypt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecryptionCapability {
    /// DVD decryption, when the worker has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub css: Option<CssDecryption>,
}

/// DVD decryption: libdvdcss, through dvdbackup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CssDecryption {
    /// Always `libdvdcss`.
    pub library: String,
    /// The library's version, as the image recorded it.
    pub version: String,
    /// Always `dvdbackup`.
    pub tool: String,
    /// The tool's Debian package version.
    pub tool_version: String,
}

/// Why a record that is present was not believed.
#[derive(Debug, thiserror::Error)]
pub enum DecryptionRecordError {
    /// It could not be read.
    #[error("the decryption record could not be read: {0}")]
    Read(#[from] std::io::Error),
    /// It is larger than any record the image writes.
    #[error("the decryption record is larger than {MAX_RECORD_BYTES} bytes")]
    TooLarge,
    /// It is not the record the image writes.
    #[error("the decryption record is malformed: {0}")]
    Malformed(String),
    /// It names something that is not installed where it should be.
    #[error("the decryption record names {0}, which is not installed")]
    Missing(&'static str),
}

/// The record as the image writes it. Unknown fields, such as `aacs`, are
/// ignored.
#[derive(Debug, Deserialize)]
struct Record {
    css: Option<CssDecryption>,
}

/// Detect decryption in the running image.
///
/// # Errors
///
/// [`DecryptionRecordError`] when a record is present but cannot be believed;
/// the worker then reports no decryption and says why in its log.
pub fn detect() -> Result<Option<DecryptionCapability>, DecryptionRecordError> {
    detect_in(Path::new("/"))
}

/// Detect decryption under `root`, which is `/` except in tests.
///
/// `Ok(None)` when there is no record, which is the published image.
///
/// # Errors
///
/// As [`detect`].
pub fn detect_in(root: &Path) -> Result<Option<DecryptionCapability>, DecryptionRecordError> {
    let file = match std::fs::File::open(root.join(RECORD)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(DecryptionRecordError::TooLarge);
    }
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|error| DecryptionRecordError::Malformed(error.to_string()))?;

    let Some(css) = record.css else {
        return Ok(None);
    };
    if css.library != "libdvdcss" {
        return Err(DecryptionRecordError::Malformed(
            "css.library is not libdvdcss".to_owned(),
        ));
    }
    if css.tool != "dvdbackup" {
        return Err(DecryptionRecordError::Malformed(
            "css.tool is not dvdbackup".to_owned(),
        ));
    }
    for (name, version) in [
        ("css.version", &css.version),
        ("css.tool_version", &css.tool_version),
    ] {
        if !plausible_version(version) {
            return Err(DecryptionRecordError::Malformed(format!(
                "{name} is not a version"
            )));
        }
    }
    if !is_file(&root.join(LIBDVDCSS)) {
        return Err(DecryptionRecordError::Missing("libdvdcss"));
    }
    if !is_file(&root.join(DVDBACKUP)) {
        return Err(DecryptionRecordError::Missing("dvdbackup"));
    }
    Ok(Some(DecryptionCapability { css: Some(css) }))
}

/// A regular file, following links: the library is a link to its full
/// version, as `ldconfig` leaves it.
fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// A Debian or upstream version: short, and only the characters versions use.
fn plausible_version(version: &str) -> bool {
    !version.is_empty()
        && version.chars().count() <= MAX_VERSION_CHARS
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '~' | ':'))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"css":{"library":"libdvdcss","version":"1.6.0","tool":"dvdbackup","tool_version":"0.4.2-4.1"},"aacs":{"library":"libaacs","version":"0.11.1-2","bdplus_version":"0.2.0-3"}}"#;

    fn image(record: Option<&str>, library: bool, tool: bool) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("test step");
        let put = |relative: &str, contents: &[u8]| {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("test step")).expect("test step");
            std::fs::write(path, contents).expect("test step");
        };
        if let Some(record) = record {
            put(RECORD, record.as_bytes());
        }
        if library {
            put("usr/local/lib/libdvdcss.so.2.4.0", b"\x7fELF");
            std::os::unix::fs::symlink("libdvdcss.so.2.4.0", root.path().join(LIBDVDCSS))
                .expect("test step");
        }
        if tool {
            put(DVDBACKUP, b"\x7fELF");
        }
        root
    }

    #[test]
    fn the_published_image_has_none() {
        let root = image(None, false, false);
        assert_eq!(detect_in(root.path()).expect("test step"), None);
    }

    #[test]
    fn the_opt_in_image_reports_css() {
        let root = image(Some(GOOD), true, true);
        let found = detect_in(root.path())
            .expect("test step")
            .expect("test step");
        assert_eq!(
            found.css,
            Some(CssDecryption {
                library: "libdvdcss".to_owned(),
                version: "1.6.0".to_owned(),
                tool: "dvdbackup".to_owned(),
                tool_version: "0.4.2-4.1".to_owned(),
            })
        );
        // AACS is in the record but not reported: there is no tool for it.
        let json = serde_json::to_value(&found).expect("test step");
        assert!(json.get("aacs").is_none(), "{json}");
    }

    #[test]
    fn a_record_without_the_library_is_not_believed() {
        let root = image(Some(GOOD), false, true);
        assert!(matches!(
            detect_in(root.path()),
            Err(DecryptionRecordError::Missing("libdvdcss"))
        ));
    }

    #[test]
    fn a_record_without_the_tool_is_not_believed() {
        let root = image(Some(GOOD), true, false);
        assert!(matches!(
            detect_in(root.path()),
            Err(DecryptionRecordError::Missing("dvdbackup"))
        ));
    }

    #[test]
    fn a_dangling_library_link_is_not_a_library() {
        let root = image(Some(GOOD), true, true);
        std::fs::remove_file(root.path().join("usr/local/lib/libdvdcss.so.2.4.0"))
            .expect("test step");
        assert!(matches!(
            detect_in(root.path()),
            Err(DecryptionRecordError::Missing("libdvdcss"))
        ));
    }

    #[test]
    fn a_record_without_css_reports_nothing() {
        let root = image(Some(r#"{"aacs":{"library":"libaacs"}}"#), true, true);
        assert_eq!(detect_in(root.path()).expect("test step"), None);
    }

    #[test]
    fn other_libraries_and_tools_are_refused() {
        for record in [
            GOOD.replace(r#""library":"libdvdcss""#, r#""library":"/tmp/evil.so""#),
            GOOD.replace(r#""tool":"dvdbackup""#, r#""tool":"sh""#),
        ] {
            let root = image(Some(&record), true, true);
            assert!(
                matches!(
                    detect_in(root.path()),
                    Err(DecryptionRecordError::Malformed(_))
                ),
                "{record}"
            );
        }
    }

    #[test]
    fn versions_are_bounded_and_plain() {
        for version in ["", "1.6.0; rm -rf /", "1.6.0\n", &"9".repeat(65)] {
            let record = GOOD.replace(
                r#""version":"1.6.0""#,
                &format!(
                    r#""version":{}"#,
                    serde_json::to_string(version).expect("test step")
                ),
            );
            let root = image(Some(&record), true, true);
            assert!(
                matches!(
                    detect_in(root.path()),
                    Err(DecryptionRecordError::Malformed(_))
                ),
                "{version:?}"
            );
        }
        assert!(plausible_version("1:0.4.2-4.1+b1~bpo12"));
    }

    #[test]
    fn malformed_and_oversized_records_are_refused() {
        let root = image(Some("not json"), true, true);
        assert!(matches!(
            detect_in(root.path()),
            Err(DecryptionRecordError::Malformed(_))
        ));
        let huge = format!("{GOOD}{}", " ".repeat(5000));
        let root = image(Some(&huge), true, true);
        assert!(matches!(
            detect_in(root.path()),
            Err(DecryptionRecordError::TooLarge)
        ));
    }
}
