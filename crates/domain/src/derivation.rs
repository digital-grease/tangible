// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What a derivative is, before anything runs.
//!
//! A derivative is a new artifact made from an existing one by a fixed tool:
//! a CHD made from a CUE/BIN, say. The original is never touched; the
//! derivative gets its own manifest whose lineage says where it came from.
//!
//! This module decides two things without any IO:
//!
//! - **whether a transformation applies** to an artifact of a given format and
//!   media family, and why not when it does not;
//! - **the fingerprint** that makes asking twice return the first result. It
//!   covers the parent's content, the transformation, every option with its
//!   default filled in, and the tool and its version. A new tool version is a
//!   new fingerprint, so an upgrade can produce a fresh derivative beside the
//!   old one rather than being silently answered by it.
//!
//! Options are typed and closed. Nothing here, and nothing a caller can send,
//! becomes an argument to a program except through a value this module
//! validated.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::digest::Sha256Digest;
use crate::enums::{ArtifactFormat, ChdCodec, MediaFamily, Transformation};
use crate::logical_path::LogicalPath;
use crate::manifest::ToolRef;

/// The schema the fingerprint is computed over. Changing what goes into a
/// fingerprint changes this, so old and new fingerprints can never collide.
pub const FINGERPRINT_SCHEMA: &str = "org.tangible.derivation/v1";

/// The program every current transformation runs.
pub const CHDMAN: &str = "chdman";

/// Why a transformation cannot be applied to an artifact.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotApplicable {
    /// The artifact's format is not one this transformation reads.
    #[error("{transformation} does not read {format} images")]
    Format {
        /// The transformation asked for.
        transformation: Transformation,
        /// The artifact's format.
        format: ArtifactFormat,
    },
    /// An ISO whose media family is not the one the transformation is for,
    /// or is not known at all. An ISO's bytes do not say whether it was a CD
    /// or a DVD, and the two make different CHDs.
    #[error("{transformation} is for {expected} images, and this one is {actual}")]
    Media {
        /// The transformation asked for.
        transformation: Transformation,
        /// The media family it needs.
        expected: MediaFamily,
        /// The artifact's media family.
        actual: MediaFamily,
    },
}

impl Transformation {
    /// The format of what it produces.
    #[must_use]
    pub const fn output_format(self) -> ArtifactFormat {
        ArtifactFormat::Chd
    }

    /// The program that runs it.
    #[must_use]
    pub const fn tool_name(self) -> &'static str {
        CHDMAN
    }

    /// Whether it can be applied to an artifact of this format and media.
    ///
    /// # Errors
    ///
    /// [`NotApplicable`] saying why not.
    pub fn check_applies(
        self,
        format: ArtifactFormat,
        media: MediaFamily,
    ) -> Result<(), NotApplicable> {
        let media_must_be = |expected: &[MediaFamily]| {
            if expected.contains(&media) {
                Ok(())
            } else {
                Err(NotApplicable::Media {
                    transformation: self,
                    expected: expected[0],
                    actual: media,
                })
            }
        };
        match (self, format) {
            // A sheet or a table of contents is a CD by construction.
            (Self::ChdCreateCd, ArtifactFormat::CueBin | ArtifactFormat::TocBin) => Ok(()),
            (Self::ChdCreateCd, ArtifactFormat::Iso) => media_must_be(&[MediaFamily::Cd]),
            (Self::ChdCreateDvd, ArtifactFormat::Iso) => {
                media_must_be(&[MediaFamily::Dvd, MediaFamily::Bluray])
            }
            (transformation, format) => Err(NotApplicable::Format {
                transformation,
                format,
            }),
        }
    }

    /// The transformation that suits an artifact, if any.
    ///
    /// For the "make a CHD of this" request that does not name one.
    #[must_use]
    pub fn suited_to(format: ArtifactFormat, media: MediaFamily) -> Option<Self> {
        [Self::ChdCreateCd, Self::ChdCreateDvd]
            .into_iter()
            .find(|t| t.check_applies(format, media).is_ok())
    }
}

impl ChdCodec {
    /// Whether the codec is one of chdman's CD codecs, which compress whole
    /// CD frames rather than plain bytes.
    #[must_use]
    pub const fn is_cd_codec(self) -> bool {
        matches!(self, Self::Cdlz | Self::Cdzl | Self::Cdfl)
    }
}

/// The options of a CHD transformation, every one with its value spelled out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChdOptions {
    /// Codecs, in the order chdman should try them for each hunk. At most
    /// four, which is what the CHD format holds.
    pub compression: Vec<ChdCodec>,
    /// Bytes per hunk, the unit chdman compresses.
    pub hunk_bytes: u32,
}

/// Why options were refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OptionsError {
    /// No codecs, or more than the format holds.
    #[error("a CHD takes one to four codecs, not {0}")]
    CodecCount(usize),
    /// The same codec twice.
    #[error("codec {0} is listed twice")]
    DuplicateCodec(ChdCodec),
    /// A CD codec in a DVD CHD, or a plain codec in a CD CHD.
    #[error("{codec} cannot be used by {transformation}")]
    WrongCodec {
        /// The transformation.
        transformation: Transformation,
        /// The codec.
        codec: ChdCodec,
    },
    /// A hunk size the transformation cannot use.
    #[error(
        "{transformation} needs a hunk size that is a whole number of {unit}-byte units, from {unit} to 1 MiB, not {hunk_bytes}"
    )]
    HunkSize {
        /// The transformation.
        transformation: Transformation,
        /// The size asked for.
        hunk_bytes: u32,
        /// The unit it must be a multiple of.
        unit: u32,
    },
}

/// Bytes in a CD frame as a CHD stores it: 2352 of sector and 96 of
/// subchannel.
const CD_FRAME_BYTES: u32 = 2448;

/// Bytes in a DVD sector.
const DVD_SECTOR_BYTES: u32 = 2048;

/// Largest hunk accepted. chdman's defaults are far smaller; this only stops
/// an absurd value.
const MAX_HUNK_BYTES: u32 = 1 << 20;

impl ChdOptions {
    /// chdman's own defaults for a transformation, written out so they are
    /// part of the fingerprint rather than whatever a later chdman decides.
    #[must_use]
    pub fn defaults(transformation: Transformation) -> Self {
        match transformation {
            Transformation::ChdCreateCd => Self {
                compression: vec![ChdCodec::Cdlz, ChdCodec::Cdzl, ChdCodec::Cdfl],
                hunk_bytes: 8 * CD_FRAME_BYTES,
            },
            Transformation::ChdCreateDvd => Self {
                compression: vec![
                    ChdCodec::Lzma,
                    ChdCodec::Zlib,
                    ChdCodec::Huff,
                    ChdCodec::Flac,
                ],
                hunk_bytes: 2 * DVD_SECTOR_BYTES,
            },
        }
    }

    /// Check the options suit the transformation.
    ///
    /// # Errors
    ///
    /// [`OptionsError`] for the first problem found.
    pub fn validate(&self, transformation: Transformation) -> Result<(), OptionsError> {
        if self.compression.is_empty() || self.compression.len() > 4 {
            return Err(OptionsError::CodecCount(self.compression.len()));
        }
        let cd = transformation == Transformation::ChdCreateCd;
        for (i, codec) in self.compression.iter().enumerate() {
            if self.compression[..i].contains(codec) {
                return Err(OptionsError::DuplicateCodec(*codec));
            }
            if codec.is_cd_codec() != cd {
                return Err(OptionsError::WrongCodec {
                    transformation,
                    codec: *codec,
                });
            }
        }
        let unit = if cd { CD_FRAME_BYTES } else { DVD_SECTOR_BYTES };
        if self.hunk_bytes < unit
            || self.hunk_bytes > MAX_HUNK_BYTES
            || !self.hunk_bytes.is_multiple_of(unit)
        {
            return Err(OptionsError::HunkSize {
                transformation,
                hunk_bytes: self.hunk_bytes,
                unit,
            });
        }
        Ok(())
    }
}

/// Everything that decides a derivative's bytes, except the parent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivationSpec {
    /// What is done.
    pub transformation: Transformation,
    /// How, every option filled in.
    pub options: ChdOptions,
    /// By which program, at which version.
    pub tool: ToolRef,
}

impl DerivationSpec {
    /// A specification, with its options checked.
    ///
    /// # Errors
    ///
    /// [`OptionsError`] when the options do not suit the transformation.
    pub fn new(
        transformation: Transformation,
        options: ChdOptions,
        tool_version: impl Into<String>,
    ) -> Result<Self, OptionsError> {
        options.validate(transformation)?;
        Ok(Self {
            transformation,
            options,
            tool: ToolRef {
                name: transformation.tool_name().to_owned(),
                version: tool_version.into(),
            },
        })
    }

    /// The options as the manifest's lineage records them.
    #[must_use]
    pub fn normalized_options(&self) -> BTreeMap<String, serde_json::Value> {
        let mut map = BTreeMap::new();
        map.insert(
            "compression".to_owned(),
            serde_json::Value::Array(
                self.options
                    .compression
                    .iter()
                    .map(|c| serde_json::Value::String(c.to_string()))
                    .collect(),
            ),
        );
        map.insert(
            "hunk_bytes".to_owned(),
            serde_json::Value::from(self.options.hunk_bytes),
        );
        map
    }

    /// The fingerprint of applying this to a parent with this content.
    ///
    /// SHA-256 over a canonical JSON document: the schema, the parent's
    /// content digest, the transformation, the normalized options and the
    /// tool. Object keys are sorted, so the same request always hashes the
    /// same, on any machine.
    #[must_use]
    pub fn fingerprint(&self, parent_content: &Sha256Digest) -> String {
        let document = serde_json::json!({
            "schema": FINGERPRINT_SCHEMA,
            "parent_content": parent_content.to_hex(),
            "transformation": self.transformation.to_string(),
            "options": self.normalized_options(),
            "tool": { "name": self.tool.name, "version": self.tool.version },
        });
        // A json! value serializes with sorted keys: serde_json's map is
        // ordered unless the preserve_order feature is on, which nothing in
        // this workspace enables.
        let bytes = serde_json::to_vec(&document).unwrap_or_default();
        hex::encode(Sha256::digest(&bytes))
    }
}

/// One digest for an artifact's content: its components' paths and digests,
/// sorted by path.
///
/// Two artifacts holding the same files under the same names have the same
/// content digest, whatever their identifiers, so a derivation already made
/// from one is recognised for the other.
#[must_use]
pub fn content_digest<'a>(
    components: impl IntoIterator<Item = (&'a LogicalPath, &'a Sha256Digest)>,
) -> Sha256Digest {
    let mut entries: Vec<(&str, String)> = components
        .into_iter()
        .map(|(path, digest)| (path.as_str(), digest.to_hex()))
        .collect();
    entries.sort();
    let mut hasher = Sha256::new();
    for (path, digest) in entries {
        // NUL cannot appear in a logical path, and a digest is hex, so the
        // encoding is unambiguous.
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(digest.as_bytes());
        hasher.update([b'\n']);
    }
    Sha256Digest::from_bytes(hasher.finalize().into())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> Sha256Digest {
        Sha256Digest::from_bytes([byte; 32])
    }

    fn path(text: &str) -> LogicalPath {
        LogicalPath::parse(text).unwrap()
    }

    fn cd_spec(version: &str) -> DerivationSpec {
        DerivationSpec::new(
            Transformation::ChdCreateCd,
            ChdOptions::defaults(Transformation::ChdCreateCd),
            version,
        )
        .unwrap()
    }

    #[test]
    fn a_cd_chd_is_made_from_a_sheet_a_toc_or_a_cd_iso() {
        let cd = Transformation::ChdCreateCd;
        assert!(
            cd.check_applies(ArtifactFormat::CueBin, MediaFamily::Cd)
                .is_ok()
        );
        assert!(
            cd.check_applies(ArtifactFormat::TocBin, MediaFamily::Unknown)
                .is_ok()
        );
        assert!(
            cd.check_applies(ArtifactFormat::Iso, MediaFamily::Cd)
                .is_ok()
        );
        assert_eq!(
            cd.check_applies(ArtifactFormat::Iso, MediaFamily::Unknown),
            Err(NotApplicable::Media {
                transformation: cd,
                expected: MediaFamily::Cd,
                actual: MediaFamily::Unknown
            })
        );
        assert!(matches!(
            cd.check_applies(ArtifactFormat::Chd, MediaFamily::Cd),
            Err(NotApplicable::Format { .. })
        ));
    }

    #[test]
    fn a_dvd_chd_is_made_only_from_a_dvd_or_blu_ray_iso() {
        let dvd = Transformation::ChdCreateDvd;
        assert!(
            dvd.check_applies(ArtifactFormat::Iso, MediaFamily::Dvd)
                .is_ok()
        );
        assert!(
            dvd.check_applies(ArtifactFormat::Iso, MediaFamily::Bluray)
                .is_ok()
        );
        assert!(
            dvd.check_applies(ArtifactFormat::Iso, MediaFamily::Cd)
                .is_err()
        );
        assert!(
            dvd.check_applies(ArtifactFormat::CueBin, MediaFamily::Cd)
                .is_err()
        );
    }

    #[test]
    fn the_suited_transformation_is_chosen_from_format_and_media() {
        assert_eq!(
            Transformation::suited_to(ArtifactFormat::TocBin, MediaFamily::Cd),
            Some(Transformation::ChdCreateCd)
        );
        assert_eq!(
            Transformation::suited_to(ArtifactFormat::Iso, MediaFamily::Dvd),
            Some(Transformation::ChdCreateDvd)
        );
        assert_eq!(
            Transformation::suited_to(ArtifactFormat::Iso, MediaFamily::Unknown),
            None
        );
    }

    #[test]
    fn the_defaults_are_valid_and_bad_options_are_refused() {
        for t in [Transformation::ChdCreateCd, Transformation::ChdCreateDvd] {
            assert_eq!(ChdOptions::defaults(t).validate(t), Ok(()));
        }
        let cd = Transformation::ChdCreateCd;
        let mut options = ChdOptions::defaults(cd);
        options.compression = vec![ChdCodec::Lzma];
        assert!(matches!(
            options.validate(cd),
            Err(OptionsError::WrongCodec { .. })
        ));
        options.compression = vec![ChdCodec::Cdlz, ChdCodec::Cdlz];
        assert_eq!(
            options.validate(cd),
            Err(OptionsError::DuplicateCodec(ChdCodec::Cdlz))
        );
        options.compression = vec![];
        assert_eq!(options.validate(cd), Err(OptionsError::CodecCount(0)));
        options = ChdOptions::defaults(cd);
        options.hunk_bytes = 4096;
        assert!(matches!(
            options.validate(cd),
            Err(OptionsError::HunkSize { .. })
        ));
    }

    #[test]
    fn the_fingerprint_is_stable_and_covers_everything_that_decides_the_bytes() {
        let parent = digest(1);
        let base = cd_spec("0.251").fingerprint(&parent);
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            cd_spec("0.251").fingerprint(&parent),
            "same request, same hash"
        );

        assert_ne!(
            base,
            cd_spec("0.251").fingerprint(&digest(2)),
            "parent content"
        );
        assert_ne!(base, cd_spec("0.252").fingerprint(&parent), "tool version");
        let mut options = ChdOptions::defaults(Transformation::ChdCreateCd);
        options.compression.reverse();
        let reordered = DerivationSpec::new(Transformation::ChdCreateCd, options, "0.251")
            .unwrap()
            .fingerprint(&parent);
        assert_ne!(base, reordered, "codec order is a preference, so it counts");
    }

    #[test]
    fn the_fingerprint_does_not_drift() {
        // Pinned: a change here means every stored fingerprint stops
        // matching, so it must come with a new FINGERPRINT_SCHEMA.
        assert_eq!(
            cd_spec("0.251").fingerprint(&digest(0xab)),
            "465188990d420155c4e051e63374fcd9965b62fe304525f78cdcaeb7619a677e"
        );
    }

    #[test]
    fn content_digest_depends_on_paths_and_digests_not_their_order() {
        let a = (path("disc.cue"), digest(1));
        let b = (path("disc.bin"), digest(2));
        let forward = content_digest([(&a.0, &a.1), (&b.0, &b.1)]);
        let backward = content_digest([(&b.0, &b.1), (&a.0, &a.1)]);
        assert_eq!(forward, backward);
        let renamed = (path("other.bin"), digest(2));
        assert_ne!(
            forward,
            content_digest([(&a.0, &a.1), (&renamed.0, &renamed.1)])
        );
    }
}
