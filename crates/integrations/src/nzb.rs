// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! An NZB an operator supplied, for a Usenet client (docs/plans/acquisition.md
//! A5).
//!
//! Tangible does not parse the XML: the client does, and refuses or fails
//! what it cannot read, which the import then reports. What is checked here
//! is that the file is within bounds and looks like an NZB at all, so a
//! picture or a torrent sent by mistake is refused at once.
//!
//! An NZB has no identity of its own the way a torrent has an infohash, and
//! SABnzbd adds the same NZB twice as two jobs, so Tangible names a job by
//! the SHA-256 of the file: the same file submitted again is found, not
//! downloaded again. The client is handed the file under a name made from
//! that digest, never the operator's file name.

use sha2::{Digest as _, Sha256};

/// The largest NZB accepted. One for a full Blu-ray runs to a few
/// megabytes.
pub const MAX_NZB_BYTES: usize = 16 * 1024 * 1024;

/// Why an NZB was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NzbError {
    /// Larger than [`MAX_NZB_BYTES`].
    #[error("an NZB is at most {MAX_NZB_BYTES} bytes")]
    TooLarge,
    /// Not an NZB.
    #[error("not an NZB file: {0}")]
    NotAnNzb(&'static str),
}

/// An NZB within bounds, named by its SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nzb {
    sha256: String,
}

impl Nzb {
    /// Check `bytes` and name them.
    ///
    /// # Errors
    ///
    /// [`NzbError`] saying why not.
    pub fn check(bytes: &[u8]) -> Result<Self, NzbError> {
        if bytes.len() > MAX_NZB_BYTES {
            return Err(NzbError::TooLarge);
        }
        if bytes.is_empty() {
            return Err(NzbError::NotAnNzb("it is empty"));
        }
        // XML is text: a NUL byte means a binary file sent by mistake.
        if bytes.contains(&0) {
            return Err(NzbError::NotAnNzb("it is not text"));
        }
        let lower = bytes.to_ascii_lowercase();
        let has = |needle: &[u8]| lower.windows(needle.len()).any(|w| w == needle);
        if !has(b"<nzb") {
            return Err(NzbError::NotAnNzb("it has no <nzb> element"));
        }
        if !has(b"<segment") {
            return Err(NzbError::NotAnNzb("it names no segments to download"));
        }
        Ok(Self {
            sha256: hex::encode(Sha256::digest(bytes)),
        })
    }

    /// Its SHA-256, in hex: how Tangible tells one NZB from another.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The file name it is handed to a client under, and so the name of the
    /// client's job and folder: `tangible-` and the digest's first twelve
    /// hex digits.
    #[must_use]
    pub fn job_name(&self) -> String {
        format!("tangible-{}", &self.sha256[..12])
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn probe() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/tool-output/sabnzbd/probe.nzb"
        ))
        .unwrap()
    }

    #[test]
    fn an_nzb_is_named_by_its_digest() {
        let nzb = Nzb::check(&probe()).unwrap();
        assert_eq!(nzb.sha256(), hex::encode(Sha256::digest(probe())));
        assert_eq!(nzb.job_name(), format!("tangible-{}", &nzb.sha256()[..12]));
    }

    #[test]
    fn what_is_not_an_nzb_is_refused_saying_why() {
        for (bytes, says) in [
            (Vec::new(), "empty"),
            (b"d4:infod6:pieces0:ee".to_vec(), "<nzb>"),
            (b"<?xml version=\"1.0\"?><nzb></nzb>".to_vec(), "segments"),
            (b"<NZB>\0<segment/>".to_vec(), "not text"),
        ] {
            let error = Nzb::check(&bytes).expect_err("refused");
            assert!(error.to_string().contains(says), "{error}");
        }
        assert_eq!(
            Nzb::check(&vec![b' '; MAX_NZB_BYTES + 1]),
            Err(NzbError::TooLarge)
        );
        assert!(Nzb::check(b"<NZB><Segment>x</Segment></NZB>").is_ok());
    }
}
