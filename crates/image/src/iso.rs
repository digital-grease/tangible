// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! ISO 9660 and UDF detection and structural inspection.
//!
//! Deliberately pure: this operates on a byte slice and a declared total
//! length, performs no I/O, and allocates nothing proportional to attacker
//! input. That makes it directly fuzzable and lets every structural case be
//! tested from a literal, without constructing gigabyte files.
//!
//! Two rules shape the design.
//!
//! **Structure decides, not the filename.** A `.iso` extension raises
//! confidence and nothing more; it can never overrule what the bytes say. An
//! image whose descriptors do not parse is not an ISO merely because of its
//! name, and one that parses cleanly is an ISO even if it is called
//! `disc.bin`.
//!
//! **Nothing is repaired.** A truncated or internally inconsistent image is
//! reported with warnings and preserved exactly as received. Silently
//! correcting a field would destroy the evidence that something is wrong with
//! the dump, which for a preservation tool is the one thing worth knowing.

use std::fmt::Write as _;

/// Bytes reserved before the first volume descriptor: sixteen 2048-byte
/// sectors, unused by ISO 9660 itself and where a boot sector lives on
/// bootable media.
pub const SYSTEM_AREA_BYTES: usize = 16 * LOGICAL_SECTOR_BYTES;

/// The logical sector size ISO 9660 descriptors are laid out in.
///
/// Distinct from the volume's own declared block size, which the primary
/// descriptor states and which this parser reads rather than assumes.
pub const LOGICAL_SECTOR_BYTES: usize = 2048;

/// The same value where a declared block size is being compared, avoiding a
/// lossy cast on every comparison.
const LOGICAL_SECTOR_U32: u32 = 2048;

/// Identifier every ISO 9660 volume descriptor carries.
const STANDARD_IDENTIFIER: &[u8] = b"CD001";

/// How many descriptors to walk before giving up.
///
/// A real image has a handful. The bound stops a crafted image with no
/// terminator from walking to the end of a large file.
const MAX_DESCRIPTORS: usize = 64;

/// How much of the file this inspector needs to see.
///
/// Enough for the system area plus [`MAX_DESCRIPTORS`] descriptors, so a
/// caller can read a bounded prefix rather than the whole image.
pub const REQUIRED_PREFIX_BYTES: usize =
    SYSTEM_AREA_BYTES + (MAX_DESCRIPTORS * LOGICAL_SECTOR_BYTES);

/// A structural observation that does not by itself invalidate the image.
///
/// Warnings are the useful output of this parser. Real preservation dumps are
/// frequently odd in ways that matter to an operator deciding whether to trust
/// a disc, and flattening them to a pass/fail verdict throws that away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsoWarning {
    /// The file is shorter than the volume claims to be.
    ///
    /// The strongest available signal that a dump is incomplete: the image
    /// itself states how many blocks it should contain.
    Truncated {
        /// Bytes the volume declares.
        declared_bytes: u64,
        /// Bytes actually present.
        actual_bytes: u64,
    },

    /// The file is longer than the volume claims.
    ///
    /// Common and usually benign (padding, or a dump that captured trailing
    /// sectors), but worth surfacing because it can also mean two images were
    /// concatenated.
    TrailingData {
        /// Bytes the volume declares.
        declared_bytes: u64,
        /// Bytes actually present.
        actual_bytes: u64,
    },

    /// A both-endian field disagrees with itself.
    ///
    /// ISO 9660 stores integers twice, little-endian then big-endian. A
    /// disagreement cannot happen in a correctly written image, so it is
    /// strong evidence of corruption or of a generator that got it wrong.
    EndianMismatch {
        /// Which field.
        field: &'static str,
        /// Value read little-endian.
        little_endian: u64,
        /// Value read big-endian.
        big_endian: u64,
    },

    /// The declared block size is not 2048.
    UnusualBlockSize {
        /// The size declared.
        block_size: u32,
    },

    /// The file length is not a whole number of blocks.
    PartialTrailingBlock {
        /// Bytes left over.
        remainder: u64,
    },

    /// The descriptor sequence ran to the bound without a terminator.
    NoTerminator,

    /// A descriptor carried an unexpected version byte.
    UnexpectedDescriptorVersion {
        /// Descriptor type.
        descriptor_type: u8,
        /// Version found.
        version: u8,
    },

    /// The filename extension disagrees with what the bytes say.
    ///
    /// Recorded rather than acted on. The extension never wins.
    ExtensionDisagreesWithStructure {
        /// The extension seen.
        extension: String,
    },
}

impl std::fmt::Display for IsoWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated {
                declared_bytes,
                actual_bytes,
            } => write!(
                f,
                "image is truncated: the volume declares {declared_bytes} bytes but only \
                 {actual_bytes} are present"
            ),
            Self::TrailingData {
                declared_bytes,
                actual_bytes,
            } => write!(
                f,
                "image carries {} bytes beyond the {declared_bytes} the volume declares",
                actual_bytes.saturating_sub(*declared_bytes)
            ),
            Self::EndianMismatch {
                field,
                little_endian,
                big_endian,
            } => write!(
                f,
                "{field} disagrees with itself: {little_endian} little-endian versus \
                 {big_endian} big-endian"
            ),
            Self::UnusualBlockSize { block_size } => {
                write!(f, "declared block size is {block_size}, not the usual 2048")
            }
            Self::PartialTrailingBlock { remainder } => {
                write!(f, "image ends with a partial block of {remainder} bytes")
            }
            Self::NoTerminator => {
                write!(f, "descriptor sequence has no terminator within the bound")
            }
            Self::UnexpectedDescriptorVersion {
                descriptor_type,
                version,
            } => write!(
                f,
                "descriptor of type {descriptor_type} has version {version}, expected 1"
            ),
            Self::ExtensionDisagreesWithStructure { extension } => write!(
                f,
                "filename extension {extension:?} does not match the detected structure"
            ),
        }
    }
}

/// A filesystem recognised inside the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemEvidence {
    /// Filesystem name, e.g. `iso9660` or `udf`.
    pub name: &'static str,
    /// Version, when the structure records one.
    pub version: Option<String>,
    /// What identified it.
    pub evidence: String,
}

/// Fields read from the primary volume descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrimaryVolume {
    /// Volume identifier, trimmed.
    ///
    /// Often the only human-meaningful naming an image carries, which makes it
    /// worth recording on every import even though it is a weak identifier.
    pub volume_id: String,
    /// System identifier, trimmed.
    pub system_id: String,
    /// Publisher, trimmed.
    pub publisher: String,
    /// Data preparer, trimmed.
    pub data_preparer: String,
    /// Application that produced the image, trimmed.
    pub application: String,
    /// Creation timestamp, as recorded, uninterpreted.
    pub created: String,
    /// Blocks the volume declares.
    pub block_count: u64,
    /// Block size the volume declares.
    pub block_size: u32,
}

/// What inspection concluded.
#[derive(Debug, Clone, PartialEq)]
pub struct IsoInspection {
    /// Whether an ISO 9660 descriptor sequence was found at all.
    pub is_iso9660: bool,
    /// Primary volume descriptor fields, when one was present.
    pub primary: Option<PrimaryVolume>,
    /// Filesystems recognised.
    pub filesystems: Vec<FilesystemEvidence>,
    /// Descriptor types seen, in order.
    pub descriptor_types: Vec<u8>,
    /// Structural observations.
    pub warnings: Vec<IsoWarning>,
    /// Human-readable evidence for the verdict.
    ///
    /// A parser that returns only a guess cannot be argued with; this is what
    /// lets an operator see why the tool concluded what it did.
    pub evidence: Vec<String>,
    /// Confidence from 0 to 1. Diagnostic, never a substitute for validation.
    pub confidence: f32,
}

impl IsoInspection {
    /// Whether anything suggests the image is incomplete or damaged.
    #[must_use]
    pub fn has_integrity_warning(&self) -> bool {
        self.warnings.iter().any(|warning| {
            matches!(
                warning,
                IsoWarning::Truncated { .. } | IsoWarning::EndianMismatch { .. }
            )
        })
    }
}

/// Read a both-endian 32-bit field, reporting disagreement.
fn both_endian_u32(bytes: &[u8], field: &'static str, warnings: &mut Vec<IsoWarning>) -> u32 {
    // Guarded by the caller's length check; the slices below are in range.
    let little = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let big = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if little != big {
        warnings.push(IsoWarning::EndianMismatch {
            field,
            little_endian: u64::from(little),
            big_endian: u64::from(big),
        });
    }
    // Prefer little-endian: it is what every real-world reader uses, so
    // agreeing with them matters more than picking the "correct" half.
    little
}

/// Read a both-endian 16-bit field, reporting disagreement.
fn both_endian_u16(bytes: &[u8], field: &'static str, warnings: &mut Vec<IsoWarning>) -> u16 {
    let little = u16::from_le_bytes([bytes[0], bytes[1]]);
    let big = u16::from_be_bytes([bytes[2], bytes[3]]);
    if little != big {
        warnings.push(IsoWarning::EndianMismatch {
            field,
            little_endian: u64::from(little),
            big_endian: u64::from(big),
        });
    }
    little
}

/// Trim an ISO 9660 padded text field.
///
/// Fields are space-padded to a fixed width. Control characters are dropped
/// rather than preserved: this text reaches logs and the web UI, and a volume
/// identifier is attacker-influenced on an untrusted image.
fn text_field(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        let ch = byte as char;
        if !ch.is_control() {
            out.push(ch);
        }
    }
    out.trim().to_owned()
}

/// Inspect the head of a candidate ISO image.
///
/// `prefix` should be the first [`REQUIRED_PREFIX_BYTES`] of the file, or the
/// whole file when it is shorter. `total_bytes` is the true length, which is
/// what makes the truncation check possible from a bounded read.
///
/// `extension_hint` is the lowercase filename extension, if any. It only
/// adjusts confidence.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn inspect(prefix: &[u8], total_bytes: u64, extension_hint: Option<&str>) -> IsoInspection {
    let mut warnings = Vec::new();
    let mut evidence = Vec::new();
    let mut filesystems = Vec::new();
    let mut descriptor_types = Vec::new();
    let mut primary = None;
    let mut saw_terminator = false;
    let mut saw_any_descriptor = false;

    if prefix.len() < SYSTEM_AREA_BYTES + LOGICAL_SECTOR_BYTES {
        evidence.push(format!(
            "file is {} bytes, too short to contain a volume descriptor at offset {}",
            prefix.len(),
            SYSTEM_AREA_BYTES
        ));
        return IsoInspection {
            is_iso9660: false,
            primary: None,
            filesystems,
            descriptor_types,
            warnings,
            evidence,
            confidence: 0.0,
        };
    }

    for index in 0..MAX_DESCRIPTORS {
        let start = SYSTEM_AREA_BYTES + index * LOGICAL_SECTOR_BYTES;
        let Some(descriptor) = prefix.get(start..start + LOGICAL_SECTOR_BYTES) else {
            break;
        };

        let identifier = &descriptor[1..6];
        // The UDF recognition sequence shares this layout but uses its own
        // identifiers, so both are read from the same walk.
        match identifier {
            b"BEA01" => {
                evidence.push("UDF beginning extended area descriptor".to_owned());
                continue;
            }
            b"NSR02" | b"NSR03" => {
                let version = if identifier == b"NSR03" {
                    "2.50+"
                } else {
                    "1.50"
                };
                filesystems.push(FilesystemEvidence {
                    name: "udf",
                    version: Some(version.to_owned()),
                    evidence: format!(
                        "{} structure identifier in the volume recognition sequence",
                        String::from_utf8_lossy(identifier)
                    ),
                });
                continue;
            }
            b"TEA01" => {
                evidence.push("UDF terminating extended area descriptor".to_owned());
                continue;
            }
            b"CD001" => {}
            _ => {
                // Anything else ends the sequence. Descriptors are contiguous,
                // so an unrecognised identifier means we have walked past them.
                break;
            }
        }

        saw_any_descriptor = true;
        let descriptor_type = descriptor[0];
        let version = descriptor[6];
        descriptor_types.push(descriptor_type);

        if version != 1 {
            warnings.push(IsoWarning::UnexpectedDescriptorVersion {
                descriptor_type,
                version,
            });
        }

        match descriptor_type {
            // Boot record.
            0 => evidence.push("boot record descriptor".to_owned()),
            // Primary volume descriptor.
            1 => {
                let block_count = u64::from(both_endian_u32(
                    &descriptor[80..88],
                    "volume space size",
                    &mut warnings,
                ));
                let block_size = u32::from(both_endian_u16(
                    &descriptor[128..132],
                    "logical block size",
                    &mut warnings,
                ));

                primary = Some(PrimaryVolume {
                    volume_id: text_field(&descriptor[40..72]),
                    system_id: text_field(&descriptor[8..40]),
                    publisher: text_field(&descriptor[318..446]),
                    data_preparer: text_field(&descriptor[446..574]),
                    application: text_field(&descriptor[574..702]),
                    created: text_field(&descriptor[813..830]),
                    block_count,
                    block_size,
                });

                filesystems.push(FilesystemEvidence {
                    name: "iso9660",
                    version: None,
                    evidence: "primary volume descriptor at sector 16".to_owned(),
                });
                evidence.push("primary volume descriptor".to_owned());
            }
            // Supplementary or enhanced: Joliet lives here.
            2 => {
                // Joliet announces itself with an escape sequence selecting
                // UCS-2. Checking for it distinguishes a Joliet image from a
                // plain supplementary descriptor.
                let escapes = &descriptor[88..120];
                let joliet = escapes
                    .windows(3)
                    .any(|window| matches!(window, [0x25, 0x2f, 0x40 | 0x43 | 0x45]));
                if joliet {
                    filesystems.push(FilesystemEvidence {
                        name: "joliet",
                        version: None,
                        evidence: "UCS-2 escape sequence in a supplementary descriptor".to_owned(),
                    });
                }
                evidence.push("supplementary volume descriptor".to_owned());
            }
            // Volume partition descriptor.
            3 => evidence.push("volume partition descriptor".to_owned()),
            255 => {
                saw_terminator = true;
                break;
            }
            other => {
                let mut note = String::new();
                let _ = write!(note, "descriptor of unrecognised type {other}");
                evidence.push(note);
            }
        }
    }

    if !saw_any_descriptor {
        evidence.push(format!(
            "no {} identifier at offset {SYSTEM_AREA_BYTES}",
            String::from_utf8_lossy(STANDARD_IDENTIFIER)
        ));
        let mut inspection = IsoInspection {
            is_iso9660: false,
            primary: None,
            filesystems,
            descriptor_types,
            warnings,
            evidence,
            confidence: 0.0,
        };
        // A UDF-only image is still a real disc image, just not ISO 9660.
        if !inspection.filesystems.is_empty() {
            inspection.confidence = 0.6;
            inspection
                .evidence
                .push("UDF recognised without an ISO 9660 descriptor".to_owned());
        }
        note_extension_disagreement(&mut inspection, extension_hint);
        return inspection;
    }

    if !saw_terminator {
        warnings.push(IsoWarning::NoTerminator);
    }

    // Size checks. These are the ones that catch a bad dump, and they are only
    // possible because the volume states its own extent.
    if let Some(volume) = &primary {
        if volume.block_size != LOGICAL_SECTOR_U32 {
            warnings.push(IsoWarning::UnusualBlockSize {
                block_size: volume.block_size,
            });
        }
        let declared = volume
            .block_count
            .saturating_mul(u64::from(volume.block_size));
        if declared > total_bytes {
            warnings.push(IsoWarning::Truncated {
                declared_bytes: declared,
                actual_bytes: total_bytes,
            });
        } else if declared > 0 && total_bytes > declared {
            warnings.push(IsoWarning::TrailingData {
                declared_bytes: declared,
                actual_bytes: total_bytes,
            });
        }
        if volume.block_size > 0 {
            let remainder = total_bytes % u64::from(volume.block_size);
            if remainder != 0 {
                warnings.push(IsoWarning::PartialTrailingBlock { remainder });
            }
        }
    }

    // Confidence reflects structure. A clean descriptor sequence with a
    // primary volume is about as sure as this gets; warnings pull it down.
    let mut confidence: f32 = if primary.is_some() { 0.95 } else { 0.7 };
    if saw_terminator {
        confidence += 0.04;
    }
    for warning in &warnings {
        confidence -= match warning {
            IsoWarning::Truncated { .. } | IsoWarning::EndianMismatch { .. } => 0.25,
            IsoWarning::NoTerminator | IsoWarning::UnusualBlockSize { .. } => 0.05,
            _ => 0.01,
        };
    }

    let mut inspection = IsoInspection {
        is_iso9660: true,
        primary,
        filesystems,
        descriptor_types,
        warnings,
        evidence,
        confidence: confidence.clamp(0.0, 1.0),
    };

    // The extension is the last thing consulted and the weakest. It nudges a
    // structurally-confirmed image upward slightly and can never rescue one
    // that failed to parse.
    match extension_hint {
        Some("iso" | "img") => {
            inspection.confidence = (inspection.confidence + 0.02).clamp(0.0, 1.0);
            inspection
                .evidence
                .push("filename extension is consistent with the structure".to_owned());
        }
        Some(_) => note_extension_disagreement(&mut inspection, extension_hint),
        None => {}
    }

    inspection
}

fn note_extension_disagreement(inspection: &mut IsoInspection, extension_hint: Option<&str>) {
    if let Some(extension) = extension_hint
        && !matches!(extension, "iso" | "img")
        && inspection.is_iso9660
    {
        inspection
            .warnings
            .push(IsoWarning::ExtensionDisagreesWithStructure {
                extension: extension.to_owned(),
            });
        inspection.evidence.push(format!(
            "structure says ISO 9660 despite the {extension:?} extension; structure wins"
        ));
    }
}
