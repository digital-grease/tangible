// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Disc-image format detection, parsing, and structural validation.
//!
//! Every input here is hostile. Detection uses signatures and structure
//! rather than filenames, parsers read bytes and never execute them, and no
//! image is ever mounted.
//!
//! In place: ISO 9660 and UDF detection, and CUE sheet and cdrdao TOC
//! parsing with track layout.

pub mod chdman;
pub mod cue;
pub mod derive;
pub mod iso;
pub mod toc;

pub use cue::{
    CdLayout, CueError, CueFile, CueSheet, CueTrack, CueWarning, LayoutError, Msf,
    ReferenceFailure, ReferenceResolution, ResolvedReference, RewriteError, TrackLayout, TrackMode,
    layout, parse, resolve_names, resolve_references, rewrite_file_names,
};
pub use iso::{FilesystemEvidence, IsoInspection, IsoWarning, PrimaryVolume, inspect};
