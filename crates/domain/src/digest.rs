// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Content digests.
//!
//! SHA-256 is the internal identity for stored file content. CRC32, MD5 and
//! SHA-1 are retained only for matching against preservation databases such as
//! Redump; they are never trusted for internal identity, which is why only
//! SHA-256 gets a first-class type here.
//!
//! This module holds and formats digest *values*. Computing them is the
//! storage layer's job: `domain` performs no I/O.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Length of a SHA-256 digest in bytes.
pub const SHA256_LEN: usize = 32;

/// Why a digest could not be parsed from text.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DigestParseError {
    /// The text was not exactly 64 hexadecimal characters.
    #[error("expected {expected} hex characters, found {found}")]
    Length {
        /// Required character count.
        expected: usize,
        /// Character count supplied.
        found: usize,
    },
    /// The text contained a character outside `[0-9a-fA-F]`.
    #[error("digest contains a non-hexadecimal character")]
    NotHex,
}

/// A SHA-256 digest of stored content.
///
/// Compared by value. Displayed and serialized as lowercase hex so that
/// manifests are byte-for-byte reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Digest([u8; SHA256_LEN]);

impl Sha256Digest {
    /// Wrap raw digest bytes produced by a hasher.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; SHA256_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; SHA256_LEN] {
        &self.0
    }

    /// Lowercase hexadecimal representation.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Sha256Digest {
    type Err = DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let expected = SHA256_LEN * 2;
        if value.len() != expected {
            return Err(DigestParseError::Length {
                expected,
                found: value.len(),
            });
        }
        let mut bytes = [0_u8; SHA256_LEN];
        hex::decode_to_slice(value, &mut bytes).map_err(|_| DigestParseError::NotHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// SHA-256 of the empty input, the standard published test vector.
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn parses_the_empty_input_vector() {
        let digest: Sha256Digest = EMPTY_SHA256.parse().expect("valid vector");
        assert_eq!(digest.to_string(), EMPTY_SHA256);
    }

    #[test]
    fn display_is_lowercase_even_when_parsed_from_uppercase() {
        let digest: Sha256Digest = EMPTY_SHA256.to_uppercase().parse().expect("valid vector");
        assert_eq!(digest.to_string(), EMPTY_SHA256);
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(
            "abcd".parse::<Sha256Digest>(),
            Err(DigestParseError::Length {
                expected: 64,
                found: 4
            })
        );
    }

    #[test]
    fn rejects_non_hex_characters() {
        let bad = "z".repeat(64);
        assert_eq!(bad.parse::<Sha256Digest>(), Err(DigestParseError::NotHex));
    }

    #[test]
    fn serde_round_trips_as_a_hex_string() {
        let digest: Sha256Digest = EMPTY_SHA256.parse().expect("valid vector");
        let json = serde_json::to_string(&digest).expect("serialize");
        assert_eq!(json, format!("\"{EMPTY_SHA256}\""));
        let back: Sha256Digest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(digest, back);
    }

    #[test]
    fn byte_round_trip_is_lossless() {
        let digest: Sha256Digest = EMPTY_SHA256.parse().expect("valid vector");
        assert_eq!(Sha256Digest::from_bytes(*digest.as_bytes()), digest);
    }
}
