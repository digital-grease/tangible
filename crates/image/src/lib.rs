// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Disc-image format detection, parsing, and structural validation.
//!
//! Every input here is hostile. Detection uses signatures and structure
//! rather than filenames, parsers read bytes and never execute them, and no
//! image is ever mounted.
//!
//! In place: ISO 9660 and UDF detection, CUE sheet and cdrdao TOC parsing
//! with track layout, CloneCD (CCD/IMG/SUB) and Alcohol 120% (MDS/MDF)
//! descriptors, CHD headers and metadata, DVD-Video `VIDEO_TS` trees, and
//! Logiqx XML DATs.

pub mod ccd;
pub mod chd;
pub mod chdman;
pub mod cue;
pub mod dat;
pub mod derive;
pub mod iso;
pub mod mds;
pub mod toc;
pub mod video_ts;

pub use cue::{
    CdLayout, CueError, CueFile, CueSheet, CueTrack, CueWarning, LayoutError, Msf,
    ReferenceFailure, ReferenceResolution, ResolvedReference, RewriteError, TrackLayout, TrackMode,
    layout, parse, resolve_names, resolve_references, rewrite_file_names,
};
pub use iso::{FilesystemEvidence, IsoInspection, IsoWarning, PrimaryVolume, inspect};
