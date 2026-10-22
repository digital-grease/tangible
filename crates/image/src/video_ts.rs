// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! DVD-Video `VIDEO_TS` trees.
//!
//! A DVD-Video disc's files, copied out of its filesystem: `VIDEO_TS.IFO`
//! (the video manager), one `VTS_nn_0.IFO` per title set, a `.BUP` backup of
//! each, and the `.VOB` files that hold menus and video. It is what a
//! decrypting rip produces (ADR 0013), and what people often have instead of
//! an image.
//!
//! Recognised from the information files' own signatures, not from the names
//! alone: a folder of files named like a DVD whose `VIDEO_TS.IFO` does not
//! begin `DVDVIDEO-VMG` is not claimed to be one. Everything here is pure:
//! the caller reads the bounded headers and sample sectors this module asks
//! for and passes the bytes in. Nothing is played, decoded or decrypted.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use tangible_domain::dvd::{MAX_TITLE_SETS, SECTOR_BYTES, css_scrambled};

/// What a video manager's information file begins with.
pub const VMG_SIGNATURE: &[u8; 12] = b"DVDVIDEO-VMG";

/// What a title set's information file begins with.
pub const VTS_SIGNATURE: &[u8; 12] = b"DVDVIDEO-VTS";

/// Bytes of each information file the caller reads: the fields used here
/// are all in the first 64.
pub const HEADER_BYTES: usize = 256;

/// Sectors of each title VOB the caller reads to see whether the video is
/// still scrambled. Not the first: a title's first sector is often clear on
/// a scrambled disc. The block reader probes the same places.
pub const SAMPLE_SECTORS: [u64; 4] = [1, 2, 16, 64];

/// Offset of the number of title sets in `VIDEO_TS.IFO`, big-endian `u16`.
const VMG_TITLE_SETS: usize = 0x3E;

/// Offset of the specification version in `VIDEO_TS.IFO`, big-endian `u16`.
const VMG_VERSION: usize = 0x20;

/// Most names listed in one warning.
const MAX_NAMES_LISTED: usize = 5;

pub use tangible_domain::dvd::DvdFile;

/// Where the tree is among the staged files, and what each file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The staged directory holding the DVD files: empty when the
    /// `VIDEO_TS` folder itself was imported, or `VIDEO_TS/` (as staged,
    /// in its own case) when the folder above it was.
    pub root: String,
    /// Whether the names matched only when case was ignored.
    pub case_folded: bool,
    /// Every DVD-Video file, by staged path.
    pub files: BTreeMap<String, DvdFile>,
    /// Every other staged file.
    pub others: Vec<String>,
}

impl Layout {
    /// The staged path of `file`, if it is there.
    #[must_use]
    pub fn path_of(&self, file: DvdFile) -> Option<&str> {
        self.files
            .iter()
            .find(|(_, found)| **found == file)
            .map(|(path, _)| path.as_str())
    }

    /// The title sets that have an information file, in order.
    #[must_use]
    pub fn title_sets(&self) -> BTreeSet<u8> {
        self.files
            .values()
            .filter_map(|file| match file {
                DvdFile::VtsIfo(set) => Some(*set),
                _ => None,
            })
            .collect()
    }

    /// Every title VOB, by staged path.
    #[must_use]
    pub fn title_vobs(&self) -> Vec<&str> {
        self.files
            .iter()
            .filter(|(_, file)| matches!(file, DvdFile::VtsTitle(..)))
            .map(|(path, _)| path.as_str())
            .collect()
    }
}

/// Find a `VIDEO_TS` tree among staged paths.
///
/// It is there when exactly one `VIDEO_TS.IFO` sits at the top, or in a
/// top-level `VIDEO_TS` folder. Names are matched exactly first; only if
/// that finds nothing are they matched ignoring case, which
/// [`Layout::case_folded`] records. `None` when there is no such file, or
/// more than one.
#[must_use]
pub fn locate(paths: &[&str]) -> Option<Layout> {
    locate_with(paths, false).or_else(|| locate_with(paths, true))
}

fn locate_with(paths: &[&str], fold: bool) -> Option<Layout> {
    // Staging never holds one path twice, but nothing here depends on that:
    // a path given twice is one file.
    let paths: BTreeSet<&str> = paths.iter().copied().collect();
    let name_of = |name: &str| {
        if fold {
            name.to_ascii_uppercase()
        } else {
            name.to_owned()
        }
    };
    let split = |path: &str| -> (String, String) {
        match path.rsplit_once('/') {
            Some((dir, name)) => (format!("{dir}/"), name.to_owned()),
            None => (String::new(), path.to_owned()),
        }
    };

    let mut roots = paths.iter().filter_map(|path| {
        let (dir, name) = split(path);
        let at_root = dir.is_empty() || name_of(&dir) == "VIDEO_TS/";
        (at_root && name_of(&name) == "VIDEO_TS.IFO").then_some(dir)
    });
    let root = roots.next()?;
    if roots.next().is_some() {
        return None;
    }

    let mut files = BTreeMap::new();
    let mut others = Vec::new();
    for path in &paths {
        let (dir, name) = split(path);
        match (dir == root)
            .then(|| DvdFile::classify(&name_of(&name)))
            .flatten()
        {
            Some(file) => {
                files.insert((*path).to_owned(), file);
            }
            None => others.push((*path).to_owned()),
        }
    }
    Some(Layout {
        root,
        case_folded: fold,
        files,
        others,
    })
}

/// What the video manager says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoManager {
    /// How many title sets it names.
    pub title_sets: u16,
    /// The DVD-Video specification version, as recorded.
    pub version: u16,
}

/// Why a tree is not DVD-Video after all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VideoTsError {
    /// `VIDEO_TS.IFO` does not begin as a video manager does.
    #[error("VIDEO_TS.IFO does not begin with DVDVIDEO-VMG")]
    NotVideoManager,
    /// It is too short to hold the fields read here.
    #[error("VIDEO_TS.IFO is shorter than its header")]
    Truncated,
}

/// Read the video manager's header.
///
/// # Errors
///
/// [`VideoTsError`] when it is not one.
pub fn read_video_manager(header: &[u8]) -> Result<VideoManager, VideoTsError> {
    if header.len() < VMG_TITLE_SETS + 2 {
        return Err(if header.starts_with(VMG_SIGNATURE) {
            VideoTsError::Truncated
        } else {
            VideoTsError::NotVideoManager
        });
    }
    if !header.starts_with(VMG_SIGNATURE) {
        return Err(VideoTsError::NotVideoManager);
    }
    let field = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
    Ok(VideoManager {
        title_sets: field(VMG_TITLE_SETS),
        version: field(VMG_VERSION),
    })
}

/// What the caller read for [`inspect`].
#[derive(Debug, Clone, Default)]
pub struct Reads {
    /// The first [`HEADER_BYTES`] of `VIDEO_TS.IFO`.
    pub video_manager: Vec<u8>,
    /// The first [`HEADER_BYTES`] of each title set's information file.
    pub title_set_headers: BTreeMap<u8, Vec<u8>>,
    /// Every file's length, by staged path.
    pub sizes: BTreeMap<String, u64>,
    /// The [`SAMPLE_SECTORS`] that could be read from each title VOB, by
    /// staged path. A VOB shorter than a sample offset has fewer.
    pub samples: BTreeMap<String, Vec<Vec<u8>>>,
}

/// What [`inspect`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspection {
    /// What the video manager says.
    pub video_manager: VideoManager,
    /// How the tree was recognised.
    pub evidence: Vec<String>,
    /// Everything structurally odd. Imported anyway, and said.
    pub warnings: Vec<String>,
    /// The files the tree cannot be read without: the video manager and
    /// each title set's information, by staged path.
    pub required_paths: Vec<String>,
    /// How sure the recognition is.
    pub confidence: f32,
}

/// Check a located tree against what it says about itself.
///
/// # Errors
///
/// [`VideoTsError`] when `VIDEO_TS.IFO` is not a video manager, in which case
/// the files are not DVD-Video and are imported as unrecognised.
pub fn inspect(layout: &Layout, reads: &Reads) -> Result<Inspection, VideoTsError> {
    let manager = read_video_manager(&reads.video_manager)?;
    let mut found = Inspection {
        video_manager: manager,
        evidence: vec![format!(
            "VIDEO_TS.IFO begins DVDVIDEO-VMG and names {} title set{}",
            manager.title_sets,
            if manager.title_sets == 1 { "" } else { "s" }
        )],
        warnings: Vec::new(),
        required_paths: layout
            .path_of(DvdFile::VmgIfo)
            .into_iter()
            .map(str::to_owned)
            .collect(),
        confidence: 0.95,
    };
    if layout.case_folded {
        found.warnings.push(
            "the DVD-Video files are named in lower or mixed case; they were matched \
             ignoring case, and kept exactly as named"
                .to_owned(),
        );
        found.confidence = found.confidence.min(0.85);
    }
    if layout.path_of(DvdFile::VmgBup).is_none() {
        found
            .warnings
            .push("VIDEO_TS.BUP, the backup of VIDEO_TS.IFO, is missing".to_owned());
    }
    count_title_sets(layout, &mut found);
    check_title_sets(layout, reads, &mut found);
    check_files(layout, reads, &mut found);
    Ok(found)
}

/// The title sets the video manager names against those that are here.
fn count_title_sets(layout: &Layout, found: &mut Inspection) {
    let declared = found.video_manager.title_sets;
    let present = layout.title_sets();
    let named: BTreeSet<u8> = (1..=declared.min(u16::from(MAX_TITLE_SETS)))
        .filter_map(|set| u8::try_from(set).ok())
        .collect();
    if declared > u16::from(MAX_TITLE_SETS) {
        found.warnings.push(format!(
            "VIDEO_TS.IFO names {declared} title sets; DVD-Video allows at most {MAX_TITLE_SETS}"
        ));
        found.confidence = found.confidence.min(0.6);
    }
    let missing: Vec<u8> = named.difference(&present).copied().collect();
    if !missing.is_empty() {
        found.warnings.push(format!(
            "VIDEO_TS.IFO names title set{} {} with no information file here; \
             that part of the disc is missing",
            if missing.len() == 1 { "" } else { "s" },
            list(missing.iter().map(|set| format!("{set:02}")))
        ));
        found.confidence = found.confidence.min(0.8);
    }
    let extra: Vec<u8> = present.difference(&named).copied().collect();
    if !extra.is_empty() {
        found.warnings.push(format!(
            "information files for title set{} {} are here, but VIDEO_TS.IFO does not \
             name {}",
            if extra.len() == 1 { "" } else { "s" },
            list(extra.iter().map(|set| format!("{set:02}"))),
            if extra.len() == 1 { "it" } else { "them" }
        ));
    }
}

/// Each title set that is here: its signature, its backup, its video.
fn check_title_sets(layout: &Layout, reads: &Reads, found: &mut Inspection) {
    let mut checked = 0_usize;
    for set in layout.title_sets() {
        let Some(path) = layout.path_of(DvdFile::VtsIfo(set)) else {
            continue;
        };
        found.required_paths.push(path.to_owned());
        match reads.title_set_headers.get(&set) {
            Some(header) if header.starts_with(VTS_SIGNATURE) => checked += 1,
            _ => found.warnings.push(format!(
                "VTS_{set:02}_0.IFO does not begin with DVDVIDEO-VTS, so it is not a title \
                 set's information file"
            )),
        }
        if layout.path_of(DvdFile::VtsBup(set)).is_none() {
            found.warnings.push(format!(
                "VTS_{set:02}_0.BUP, the backup of VTS_{set:02}_0.IFO, is missing"
            ));
        }
        let parts: BTreeSet<u8> = layout
            .files
            .values()
            .filter_map(|file| match file {
                DvdFile::VtsTitle(s, part) if *s == set => Some(*part),
                _ => None,
            })
            .collect();
        match parts.last() {
            None => found.warnings.push(format!(
                "title set {set:02} has no video: VTS_{set:02}_1.VOB is missing"
            )),
            Some(&last) => {
                let gaps: Vec<u8> = (1..last).filter(|part| !parts.contains(part)).collect();
                if !gaps.is_empty() {
                    found.warnings.push(format!(
                        "title set {set:02}'s video is incomplete: {} missing",
                        list(gaps.iter().map(|part| format!("VTS_{set:02}_{part}.VOB")))
                    ));
                }
            }
        }
    }
    if checked > 0 {
        found.evidence.push(format!(
            "{checked} title set information file{} begin{} DVDVIDEO-VTS",
            if checked == 1 { "" } else { "s" },
            if checked == 1 { "s" } else { "" }
        ));
    }
}

/// Sizes, scrambling, and files that are not DVD-Video.
fn check_files(layout: &Layout, reads: &Reads, found: &mut Inspection) {
    let ragged: Vec<&str> = layout
        .files
        .iter()
        .filter(|(path, file)| {
            matches!(
                file,
                DvdFile::VmgMenu | DvdFile::VtsMenu(_) | DvdFile::VtsTitle(..)
            ) && reads
                .sizes
                .get(*path)
                .is_some_and(|bytes| bytes % SECTOR_BYTES as u64 != 0)
        })
        .map(|(path, _)| path.as_str())
        .collect();
    if !ragged.is_empty() {
        found.warnings.push(format!(
            "{} not a whole number of 2048-byte sectors, so a copy may be cut short",
            list_is(&ragged)
        ));
    }

    let scrambled: Vec<&str> = reads
        .samples
        .iter()
        .filter(|(_, sectors)| sectors.iter().any(|sector| css_scrambled(sector)))
        .map(|(path, _)| path.as_str())
        .collect();
    if !scrambled.is_empty() {
        found.warnings.push(format!(
            "the video in {} is still CSS-scrambled: it was copied without being \
             decrypted, and will not play",
            list(scrambled.iter().map(ToString::to_string))
        ));
    }

    if !layout.others.is_empty() {
        let others: Vec<&str> = layout.others.iter().map(String::as_str).collect();
        found.warnings.push(format!(
            "{} not part of DVD-Video; kept, with no role",
            list_is(&others)
        ));
    }
}

/// `a, b and 3 more`.
fn list(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    let shown = &items[..items.len().min(MAX_NAMES_LISTED)];
    let mut out = shown.join(", ");
    if items.len() > shown.len() {
        let _ = write!(out, " and {} more", items.len() - shown.len());
    }
    out
}

/// `a is` or `a, b are`.
fn list_is(items: &[&str]) -> String {
    format!(
        "{} {}",
        list(items.iter().map(ToString::to_string)),
        if items.len() == 1 { "is" } else { "are" }
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn manager(title_sets: u16) -> Vec<u8> {
        let mut header = vec![0_u8; HEADER_BYTES];
        header[..12].copy_from_slice(VMG_SIGNATURE);
        header[VMG_VERSION..VMG_VERSION + 2].copy_from_slice(&0x0011_u16.to_be_bytes());
        header[VMG_TITLE_SETS..VMG_TITLE_SETS + 2].copy_from_slice(&title_sets.to_be_bytes());
        header
    }

    fn title_set() -> Vec<u8> {
        let mut header = vec![0_u8; HEADER_BYTES];
        header[..12].copy_from_slice(VTS_SIGNATURE);
        header
    }

    /// A clear and a scrambled video sector.
    fn sector(scrambled: bool) -> Vec<u8> {
        let mut sector = vec![0_u8; SECTOR_BYTES];
        sector[..4].copy_from_slice(&[0, 0, 1, 0xBA]);
        sector[4] = 0x44;
        sector[13] = 0xF8;
        sector[14..18].copy_from_slice(&[0, 0, 1, 0xE0]);
        sector[20] = if scrambled { 0x90 } else { 0x80 };
        sector
    }

    const WHOLE: [&str; 9] = [
        "VIDEO_TS/VIDEO_TS.BUP",
        "VIDEO_TS/VIDEO_TS.IFO",
        "VIDEO_TS/VIDEO_TS.VOB",
        "VIDEO_TS/VTS_01_0.BUP",
        "VIDEO_TS/VTS_01_0.IFO",
        "VIDEO_TS/VTS_01_1.VOB",
        "VIDEO_TS/VTS_01_2.VOB",
        "VIDEO_TS/VTS_02_0.IFO",
        "VIDEO_TS/VTS_02_1.VOB",
    ];

    fn reads(layout: &Layout, title_sets: u16) -> Reads {
        Reads {
            video_manager: manager(title_sets),
            title_set_headers: layout
                .title_sets()
                .into_iter()
                .map(|set| (set, title_set()))
                .collect(),
            sizes: layout
                .files
                .keys()
                .map(|path| (path.clone(), 4 * SECTOR_BYTES as u64))
                .collect(),
            samples: layout
                .title_vobs()
                .into_iter()
                .map(|path| (path.to_owned(), vec![sector(false), sector(false)]))
                .collect(),
        }
    }

    #[test]
    fn names_are_classified_as_dvd_video_spells_them() {
        assert_eq!(DvdFile::classify("VIDEO_TS.IFO"), Some(DvdFile::VmgIfo));
        assert_eq!(DvdFile::classify("VTS_01_0.IFO"), Some(DvdFile::VtsIfo(1)));
        assert_eq!(DvdFile::classify("VTS_99_0.BUP"), Some(DvdFile::VtsBup(99)));
        assert_eq!(
            DvdFile::classify("VTS_12_0.VOB"),
            Some(DvdFile::VtsMenu(12))
        );
        assert_eq!(
            DvdFile::classify("VTS_03_9.VOB"),
            Some(DvdFile::VtsTitle(3, 9))
        );
        for name in [
            "VTS_00_0.IFO",
            "VTS_01_1.IFO",
            "VTS_01_0.TXT",
            "VTS_1_0.IFO",
            "vts_01_0.ifo",
            "VTS_0A_1.VOB",
            "VTS_01_10.VOB",
            "VIDEO_TS.TXT",
            "",
        ] {
            assert_eq!(DvdFile::classify(name), None, "{name}");
        }
    }

    #[test]
    fn a_whole_tree_is_found_under_its_folder_or_at_the_top() {
        let layout = locate(&WHOLE).expect("found");
        assert_eq!(layout.root, "VIDEO_TS/");
        assert!(!layout.case_folded);
        assert_eq!(layout.files.len(), 9);
        assert!(layout.others.is_empty());
        assert_eq!(layout.title_sets(), BTreeSet::from([1, 2]));

        let top: Vec<String> = WHOLE
            .iter()
            .map(|path| path.trim_start_matches("VIDEO_TS/").to_owned())
            .collect();
        let top: Vec<&str> = top.iter().map(String::as_str).collect();
        let layout = locate(&top).expect("found");
        assert_eq!(layout.root, "");
        assert_eq!(layout.files.len(), 9);
    }

    #[test]
    fn a_well_formed_tree_has_no_warnings_beyond_a_missing_backup() {
        let layout = locate(&WHOLE).unwrap();
        let found = inspect(&layout, &reads(&layout, 2)).unwrap();
        assert_eq!(
            found.warnings,
            ["VTS_02_0.BUP, the backup of VTS_02_0.IFO, is missing"]
        );
        assert_eq!(found.video_manager.title_sets, 2);
        assert_eq!(
            found.required_paths,
            [
                "VIDEO_TS/VIDEO_TS.IFO",
                "VIDEO_TS/VTS_01_0.IFO",
                "VIDEO_TS/VTS_02_0.IFO"
            ]
        );
        assert!(
            found.evidence[0].contains("names 2 title sets"),
            "{:?}",
            found.evidence
        );
        assert!(found.evidence[1].contains("2 title set information files"));
    }

    #[test]
    fn nothing_without_a_video_manager_or_with_two() {
        assert_eq!(locate(&["disc.iso"]), None);
        assert_eq!(
            locate(&["VIDEO_TS/VTS_01_0.IFO", "VIDEO_TS/VTS_01_1.VOB"]),
            None
        );
        // Deeper than a top-level VIDEO_TS folder is not this disc's tree.
        assert_eq!(locate(&["MOVIE/VIDEO_TS/VIDEO_TS.IFO"]), None);
        assert_eq!(locate(&["A/VIDEO_TS.IFO"]), None);
        assert_eq!(locate(&["VIDEO_TS.IFO", "VIDEO_TS/VIDEO_TS.IFO"]), None);
    }

    #[test]
    fn a_lower_case_tree_is_found_and_said_to_be() {
        let lower: Vec<String> = WHOLE.iter().map(|path| path.to_ascii_lowercase()).collect();
        let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
        let layout = locate(&lower).expect("found ignoring case");
        assert!(layout.case_folded);
        assert_eq!(layout.root, "video_ts/");
        assert_eq!(
            layout.path_of(DvdFile::VmgIfo),
            Some("video_ts/video_ts.ifo")
        );
        let found = inspect(&layout, &reads(&layout, 2)).unwrap();
        assert!(
            found.warnings[0].contains("ignoring case"),
            "{:?}",
            found.warnings
        );
        assert!(found.confidence < 0.9);
    }

    #[test]
    fn a_file_that_only_looks_like_a_video_manager_is_not_one() {
        let layout = locate(&WHOLE).unwrap();
        let mut reads = reads(&layout, 2);
        reads.video_manager = b"MZ\x90\x00 not a DVD at all".to_vec();
        assert_eq!(inspect(&layout, &reads), Err(VideoTsError::NotVideoManager));
        reads.video_manager = VMG_SIGNATURE.to_vec();
        assert_eq!(inspect(&layout, &reads), Err(VideoTsError::Truncated));
        reads.video_manager = Vec::new();
        assert_eq!(inspect(&layout, &reads), Err(VideoTsError::NotVideoManager));
    }

    #[test]
    fn missing_and_unexpected_parts_are_said() {
        let mut paths = WHOLE.to_vec();
        paths.retain(|path| *path != "VIDEO_TS/VTS_01_1.VOB" && *path != "VIDEO_TS/VIDEO_TS.BUP");
        paths.push("VIDEO_TS/VTS_01_3.VOB");
        paths.push("VIDEO_TS/VTS_05_0.IFO");
        paths.push("VIDEO_TS/notes.txt");
        paths.push("cover.jpg");
        let layout = locate(&paths).unwrap();
        let mut reads = reads(&layout, 3);
        reads.title_set_headers.insert(5, b"garbage".to_vec());
        reads.sizes.insert(
            "VIDEO_TS/VTS_02_1.VOB".to_owned(),
            3 * SECTOR_BYTES as u64 + 100,
        );
        let found = inspect(&layout, &reads).unwrap();
        let all = found.warnings.join("\n");
        for expected in [
            "VIDEO_TS.BUP, the backup of VIDEO_TS.IFO, is missing",
            "names title set 03 with no information file here",
            "information files for title set 05 are here",
            "VTS_05_0.IFO does not begin with DVDVIDEO-VTS",
            "title set 01's video is incomplete: VTS_01_1.VOB missing",
            "title set 05 has no video",
            "VIDEO_TS/VTS_02_1.VOB is not a whole number",
            "VIDEO_TS/notes.txt, cover.jpg are not part of DVD-Video",
        ] {
            assert!(all.contains(expected), "missing {expected:?} in:\n{all}");
        }
        assert!(found.confidence <= 0.8);
    }

    #[test]
    fn scrambled_video_is_said_and_clear_video_is_not() {
        let layout = locate(&WHOLE).unwrap();
        let mut reads = reads(&layout, 2);
        reads.samples.insert(
            "VIDEO_TS/VTS_01_2.VOB".to_owned(),
            vec![sector(false), sector(true)],
        );
        let found = inspect(&layout, &reads).unwrap();
        let scrambled: Vec<&String> = found
            .warnings
            .iter()
            .filter(|warning| warning.contains("CSS-scrambled"))
            .collect();
        assert_eq!(scrambled.len(), 1, "{:?}", found.warnings);
        assert!(scrambled[0].contains("VIDEO_TS/VTS_01_2.VOB"));
        assert!(!scrambled[0].contains("VTS_01_1"));
    }

    #[test]
    fn an_absurd_title_set_count_is_bounded() {
        let layout = locate(&WHOLE).unwrap();
        let found = inspect(&layout, &reads(&layout, u16::MAX)).unwrap();
        let all = found.warnings.join("\n");
        assert!(all.contains("at most 99"), "{all}");
        assert!(all.contains("and 92 more"), "the list is bounded: {all}");
    }

    #[test]
    fn a_path_given_twice_is_one_file() {
        let mut paths = WHOLE.to_vec();
        paths.push("VIDEO_TS/VIDEO_TS.BUP");
        paths.push("notes.txt");
        paths.push("notes.txt");
        let layout = locate(&paths).unwrap();
        assert_eq!(layout.files.len(), 9);
        assert_eq!(layout.others, ["notes.txt"]);
    }

    #[test]
    fn locate_and_inspect_never_panic_on_odd_paths() {
        for paths in [
            vec!["/", "VIDEO_TS.IFO/"],
            vec![
                "VIDEO_TS/",
                "VIDEO_TS/VIDEO_TS.IFO",
                "VIDEO_TS/VTS_\u{e9}\u{e9}_0.IFO",
            ],
            vec!["VIDEO_TS.IFO", "VTS_01_0.IFO", "VTS_01_0.IFO"],
        ] {
            if let Some(layout) = locate(&paths) {
                let _ = inspect(&layout, &Reads::default());
                let _ = inspect(&layout, &reads(&layout, 1));
            }
        }
    }
}
