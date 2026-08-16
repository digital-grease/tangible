// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Disc-image format detection, parsing, and structural validation.
//!
//! Every input here is hostile. Detection uses signatures and structure
//! rather than filenames, parsers read bytes and never execute them, and no
//! image is ever mounted.
//!
//! In place: ISO 9660 and UDF detection. CUE/BIN parsing follows.

pub mod iso;

pub use iso::{FilesystemEvidence, IsoInspection, IsoWarning, PrimaryVolume, inspect};
