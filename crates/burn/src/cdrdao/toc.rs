// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Writing a cdrdao table of contents.
//!
//! cdrdao does not read CUE sheets. It reads a TOC file, which describes the
//! same disc in a different vocabulary, and this turns one into the other.
//!
//! The conversion is where the layout semantics established in E7-03 get
//! tested against something that has to be exactly right, and it is the one
//! place in this project that generates a file for another program to
//! interpret. Three rules follow from that:
//!
//! * **Everything is expressed in blocks.** cdrdao accepts lengths in bytes,
//!   in samples, or as `MM:SS:FF` where a frame is one block. The layout is
//!   already in sectors, so writing MSF everywhere means no sector-size
//!   arithmetic happens here at all, and arithmetic that does not happen
//!   cannot be wrong.
//! * **Anything not exactly expressible is refused.** A TOC that is nearly
//!   right produces a disc that is nearly right, which is a coaster.
//! * **Nothing untrusted is interpolated.** Filenames are quoted, and a name
//!   that cannot be quoted safely is refused rather than escaped, for the same
//!   reason engines take fixed argument arrays instead of a command line.
//!
//! # Where these rules come from
//!
//! The grammar was read from cdrdao's manual rather than recalled: the
//! argument lists for `DATAFILE` and `FILE`, the `START` idiom, and the block
//! size of every track mode. Three details in particular are load-bearing and
//! are not what a reasonable person would guess, so they are called out at the
//! code that depends on them.
//!
//! # What a TOC cannot carry from here
//!
//! The media catalogue number and per-track ISRCs. The descriptor parser reads
//! both, and the v1alpha1 manifest has nowhere to put them, so a disc written
//! through this path loses them. Worth fixing in the schema rather than
//! guessing at here.

use std::fmt::Write as _;

use tangible_domain::cd;

use crate::plan::{BurnPlan, PlannedTrack};

/// Why a plan could not be written as a table of contents.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TocError {
    /// The plan describes one flat image rather than a disc of tracks.
    #[error("this plan has no track layout to write a table of contents from")]
    NotATrackLayout,

    /// A mode with no known sector size.
    #[error("track {track} has the unrecognised mode {mode}")]
    UnknownTrackMode {
        /// The track.
        track: u32,
        /// The mode as the descriptor wrote it.
        mode: String,
    },

    /// A mode cdrdao has no equivalent for.
    #[error("track {track} is {mode}, which cannot be written from a track descriptor: {reason}")]
    ModeNotWritable {
        /// The track.
        track: u32,
        /// The mode as the descriptor wrote it.
        mode: String,
        /// Why not.
        reason: &'static str,
    },

    /// A track refers to an input the plan does not carry.
    #[error("track {track} names an input the plan does not have")]
    TrackInputMissing {
        /// The track.
        track: u32,
    },

    /// A data track that does not begin at the start of its file.
    ///
    /// `DATAFILE` takes a length and no offset, so a data track can only be
    /// read from the beginning of a file. The ordinary mixed-mode layout, a
    /// data track first and audio after it, satisfies this; a data track
    /// somewhere else in a shared file does not.
    #[error("track {track} is data at offset {offset} of its file, which a TOC cannot address")]
    DataTrackNotAtStartOfFile {
        /// The track.
        track: u32,
        /// Where it starts.
        offset: u64,
    },

    /// A data track whose file carries its pregap.
    ///
    /// Expressing an in-file gap needs the data statement split in two around
    /// a `START`, and `DATAFILE` cannot be split because it cannot say where
    /// to begin reading.
    #[error("track {track} is data with a pregap in its file, which a TOC cannot express")]
    DataTrackHasInFilePregap {
        /// The track.
        track: u32,
    },

    /// A generated pregap on the first track.
    ///
    /// cdrdao supplies track one's mandatory pregap itself. Declaring another
    /// would either be ignored or be added twice, and which of those happens
    /// is not something to find out on a disc.
    #[error(
        "track {track} is the first track and declares a generated pregap of {sectors} sectors"
    )]
    PregapOnFirstTrack {
        /// The track.
        track: u32,
        /// The gap it declared.
        sectors: u64,
    },

    /// An offset that is not a whole number of the track's sectors.
    #[error(
        "track {track} starts at byte {offset}, which is not a multiple of its {sector_bytes} byte sectors"
    )]
    MisalignedOffset {
        /// The track.
        track: u32,
        /// Where it starts.
        offset: u64,
        /// Bytes per sector for the mode.
        sector_bytes: u64,
    },

    /// A path that cannot be written into a quoted string safely.
    #[error("the staged path {path} cannot be named in a table of contents")]
    UnquotablePath {
        /// The offending path, as far as it can be shown.
        path: String,
    },
}

/// The disc type line, which cdrdao requires first.
fn disc_type(tracks: &[PlannedTrack]) -> &'static str {
    if tracks
        .iter()
        .any(|track| track.mode.to_ascii_uppercase().starts_with("MODE2"))
        || tracks
            .iter()
            .any(|track| track.mode.to_ascii_uppercase().starts_with("CDI"))
    {
        // Mode 2 tracks are XA, whether or not audio sits beside them.
        "CD_ROM_XA"
    } else if tracks
        .iter()
        .any(|track| track.mode.to_ascii_uppercase().starts_with("MODE1"))
    {
        "CD_ROM"
    } else {
        "CD_DA"
    }
}

/// The cdrdao track mode for a descriptor's mode.
///
/// The sizes are the reason this is a table and not a transformation of the
/// name: cdrdao's mode names carry no digits, so `MODE2/2336` and
/// `MODE2/2352` become entirely different words. Each pairing is size-checked
/// against [`tangible_domain::cd`] by a test.
fn track_mode(track: &PlannedTrack) -> Result<&'static str, TocError> {
    match track.mode.to_ascii_uppercase().as_str() {
        "AUDIO" => Ok("AUDIO"),
        "MODE1/2048" => Ok("MODE1"),
        "MODE1/2352" => Ok("MODE1_RAW"),
        "MODE2/2048" => Ok("MODE2_FORM1"),
        "MODE2/2324" => Ok("MODE2_FORM2"),
        // Form 1 and form 2 sectors mixed in one track, which is what a 2336
        // byte mode 2 track from a game disc actually holds.
        "MODE2/2336" | "CDI/2336" => Ok("MODE2_FORM_MIX"),
        "MODE2/2352" | "CDI/2352" => Ok("MODE2_RAW"),
        "CDG" => Err(TocError::ModeNotWritable {
            track: track.number,
            mode: track.mode.clone(),
            reason: "CD+G graphics live in the subchannel, which a CUE and BIN pair does not carry",
        }),
        _ => Err(TocError::UnknownTrackMode {
            track: track.number,
            mode: track.mode.clone(),
        }),
    }
}

/// Format a sector count as `MM:SS:FF`, where one frame is one block.
fn msf(sectors: u64) -> String {
    let frames = sectors % cd::FRAMES_PER_SECOND;
    let seconds = (sectors / cd::FRAMES_PER_SECOND) % 60;
    let minutes = sectors / cd::FRAMES_PER_SECOND / 60;
    format!("{minutes:02}:{seconds:02}:{frames:02}")
}

/// Quote a path for a TOC, or refuse it.
///
/// cdrdao's own lexer may well accept an escaped quote. This does not rely on
/// that: a filename carrying a quote or a newline is a filename that could
/// close the string and continue with statements of its own, and refusing is
/// both simpler to reason about and impossible to get subtly wrong. Component
/// paths reach here from a descriptor written by somebody else.
fn quote(path: &std::path::Path) -> Result<String, TocError> {
    let text = path.to_str().ok_or_else(|| TocError::UnquotablePath {
        path: path.display().to_string(),
    })?;
    if text.contains('"') || text.contains('\\') || text.chars().any(char::is_control) {
        return Err(TocError::UnquotablePath {
            path: path.display().to_string(),
        });
    }
    Ok(format!("\"{text}\""))
}

/// Write the table of contents for a plan.
///
/// The plan must already have passed [`BurnPlan::check_layout`]; this refuses
/// what that check does not cover, which is everything specific to how a TOC
/// can address a file.
///
/// # Errors
///
/// [`TocError`] for anything the format cannot express exactly.
pub fn write_toc(plan: &BurnPlan) -> Result<String, TocError> {
    if !plan.is_track_layout() {
        return Err(TocError::NotATrackLayout);
    }

    let mut document = String::new();
    let _ = writeln!(document, "{}", disc_type(&plan.tracks));

    for (position, track) in plan.tracks.iter().enumerate() {
        let mode = track_mode(track)?;
        let sector_bytes =
            u64::from(
                cd::sector_bytes(&track.mode).ok_or_else(|| TocError::UnknownTrackMode {
                    track: track.number,
                    mode: track.mode.clone(),
                })?,
            );
        let input = plan
            .inputs
            .get(track.input)
            .ok_or(TocError::TrackInputMissing {
                track: track.number,
            })?;
        if !track.file_offset_bytes.is_multiple_of(sector_bytes) {
            return Err(TocError::MisalignedOffset {
                track: track.number,
                offset: track.file_offset_bytes,
                sector_bytes,
            });
        }
        let start_block = track.file_offset_bytes / sector_bytes;
        let in_file_pregap = track.in_file_pregap();
        let generated_pregap = track.generated_pregap();
        let file = quote(&input.staged_path)?;

        if position == 0 && generated_pregap > 0 {
            return Err(TocError::PregapOnFirstTrack {
                track: track.number,
                sectors: generated_pregap,
            });
        }

        let _ = writeln!(document);
        let _ = writeln!(document, "TRACK {mode}");

        // A gap in no file, which the writer fills with silence or zeroes.
        if generated_pregap > 0 {
            let _ = writeln!(document, "PREGAP {}", msf(generated_pregap));
        }

        if track.is_audio() {
            // A gap the file carries is written as its own FILE statement,
            // then START, then the track proper. That is the form the manual
            // demonstrates, and it says where index 01 falls without anything
            // having to be counted twice.
            if in_file_pregap > 0 {
                let _ = writeln!(
                    document,
                    "FILE {file} {} {}",
                    msf(start_block),
                    msf(in_file_pregap)
                );
                let _ = writeln!(document, "START");
                let _ = writeln!(
                    document,
                    "FILE {file} {} {}",
                    msf(start_block + in_file_pregap),
                    msf(track.sector_count.saturating_sub(in_file_pregap))
                );
            } else {
                let _ = writeln!(
                    document,
                    "FILE {file} {} {}",
                    msf(start_block),
                    msf(track.sector_count)
                );
            }
        } else {
            // DATAFILE names a length and no offset, so a data track can only
            // be read from the start of its file.
            if start_block > 0 {
                return Err(TocError::DataTrackNotAtStartOfFile {
                    track: track.number,
                    offset: track.file_offset_bytes,
                });
            }
            if in_file_pregap > 0 {
                return Err(TocError::DataTrackHasInFilePregap {
                    track: track.number,
                });
            }
            let _ = writeln!(document, "DATAFILE {file} {}", msf(track.sector_count));
        }

        // Index points past the first, positioned from the start of the track
        // proper rather than from the start of its data.
        let index_one = track
            .indexes
            .iter()
            .find(|index| index.number == 1)
            .map_or(0, |index| index.relative_lba);
        for index in track.indexes.iter().filter(|index| index.number > 1) {
            let _ = writeln!(
                document,
                "INDEX {}",
                msf(index.relative_lba.saturating_sub(index_one))
            );
        }
    }

    Ok(document)
}
