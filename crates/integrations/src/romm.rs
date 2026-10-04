// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Exporting games to a RomM library.
//!
//! RomM reads a folder tree: `roms/{platform}/{game}`, where a file directly
//! under the platform folder is one game and a folder is one multi-file game,
//! so "multi-disc and cue+bin games stay whole without you declaring
//! anything" (RomM's folder-structure documentation). Tangible writes that
//! shape into a configured root, which is RomM's `roms` directory:
//!
//! ```text
//! {root}/psx/Example Game (USA)/
//!     Example Game (USA) (Disc 1).cue
//!     Example Game (USA) (Disc 1).bin
//!     Example Game (USA) (Disc 2).cue
//!     Example Game (USA) (Disc 2).bin
//!     .tangible-export.json
//! ```
//!
//! Three rules govern it:
//!
//! - **The export is a view.** Nothing in it is canonical; the library's
//!   originals are never renamed, rewritten or moved to make it.
//! - **Only managed files are touched.** Every folder Tangible writes holds
//!   a marker naming the edition and every file it wrote. A folder without a
//!   matching marker is somebody else's and is never replaced or removed,
//!   and a managed folder someone has added files to is left alone too.
//! - **A failed export leaves the previous one in place.** The new folder is
//!   built beside the old under a staging name and swapped in by rename.
//!
//! Renaming a CUE sheet's files to the RomM names means its `FILE` lines must
//! name them, so an exported sheet is a copy with those lines rewritten and
//! every other byte kept. The original sheet in the library is untouched, and
//! the marker records the rewritten copy's digest like any other file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The name of the marker file in every managed folder.
pub const MARKER: &str = ".tangible-export.json";

/// The marker's schema identifier.
pub const MARKER_SCHEMA: &str = "org.tangible.romm-export/v1";

/// Longest name a folder or file is given.
const MAX_NAME_CHARS: usize = 150;

/// The platforms a disc image can be exported to: RomM's folder slug and its
/// display name.
///
/// Taken from RomM's supported-platforms list on 2026-10-04, keeping the
/// systems whose games shipped on CD, DVD, GD-ROM or Blu-ray in a form this
/// project imports (ISO or CUE/BIN). The slug is the folder name RomM
/// matches, case-insensitively.
pub const PLATFORMS: &[(&str, &str)] = &[
    ("3do", "3DO Interactive Multiplayer"),
    ("amiga-cd", "Amiga CD"),
    ("amiga-cd32", "Amiga CD32"),
    ("apple-pippin", "Apple Pippin"),
    ("atari-jaguar-cd", "Atari Jaguar CD"),
    ("commodore-cdtv", "Commodore CDTV"),
    ("dc", "Dreamcast"),
    ("dos", "DOS"),
    ("fm-towns", "FM Towns"),
    ("laseractive", "LaserActive"),
    ("neo-geo-cd", "Neo Geo CD"),
    ("ngc", "Nintendo GameCube"),
    ("nuon", "Nuon"),
    ("pc-9800-series", "PC-9800 Series"),
    ("pc-fx", "PC-FX"),
    ("philips-cd-i", "Philips CD-i"),
    ("playdia", "Playdia"),
    ("ps2", "PlayStation 2"),
    ("ps3", "PlayStation 3"),
    ("psx", "PlayStation"),
    ("saturn", "Sega Saturn"),
    ("segacd", "Sega CD"),
    ("segacd32", "Sega CD 32X"),
    ("turbografx-cd", "TurboGrafx-CD / PC Engine CD"),
    ("wii", "Wii"),
    ("wiiu", "Wii U"),
    ("win", "Windows"),
    ("win3x", "Windows 3.x"),
    ("win9x", "Windows 9x"),
    ("xbox", "Xbox"),
    ("xbox360", "Xbox 360"),
];

/// Whether `slug` is one of [`PLATFORMS`].
#[must_use]
pub fn is_platform(slug: &str) -> bool {
    PLATFORMS.iter().any(|(known, _)| *known == slug)
}

// --- what is exported ----------------------------------------------------------------

/// One file of a disc image, as the library holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Its path inside the artifact.
    pub logical_path: String,
    /// Where its bytes are in the object store.
    pub object_path: PathBuf,
    /// Its digest, lowercase hex.
    pub sha256: String,
    /// Its size.
    pub length: u64,
}

/// One disc's image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscImage {
    /// A single image file.
    Iso(SourceFile),
    /// A CUE sheet and the files its `FILE` lines name.
    CueBin {
        /// The sheet.
        sheet: SourceFile,
        /// The sheet's bytes, which the export rewrites.
        sheet_bytes: Vec<u8>,
        /// Each name a `FILE` line declares, exactly as written, and the
        /// file it resolved to, in the order the sheet declares them.
        files: Vec<(String, SourceFile)>,
    },
}

/// One edition of a game, as it should appear in RomM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRequest {
    /// The edition.
    pub edition_id: String,
    /// The game's name.
    pub title: String,
    /// The edition's region, which RomM reads from the name.
    pub region: Option<String>,
    /// The RomM platform slug.
    pub platform: String,
    /// Its discs, in disc order.
    pub discs: Vec<DiscImage>,
}

/// Where a file's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSource {
    /// An object in the store, linked or copied as it is.
    Object(PathBuf),
    /// Bytes made for the export: a CUE sheet with its `FILE` lines
    /// rewritten.
    Written(Vec<u8>),
}

/// One file the export will hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// Its name in the game's folder.
    pub name: String,
    /// Where its bytes come from.
    pub source: FileSource,
    /// The digest of the bytes the export will hold.
    pub sha256: String,
    /// Their length.
    pub length: u64,
}

/// What one edition's export is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPlan {
    /// The edition.
    pub edition_id: String,
    /// The platform folder.
    pub platform: String,
    /// The game's folder name.
    pub folder: String,
    /// Its files.
    pub files: Vec<PlannedFile>,
}

impl ExportPlan {
    /// The folder, relative to the export root.
    #[must_use]
    pub fn relative_folder(&self) -> String {
        format!("{}/{}", self.platform, self.folder)
    }

    /// The marker this plan's folder holds.
    #[must_use]
    pub fn marker(&self) -> Marker {
        Marker {
            schema: MARKER_SCHEMA.to_owned(),
            edition_id: self.edition_id.clone(),
            files: self
                .files
                .iter()
                .map(|file| MarkedFile {
                    name: file.name.clone(),
                    sha256: file.sha256.clone(),
                    length: file.length,
                })
                .collect(),
        }
    }
}

/// Why an edition cannot be exported as it stands.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// No platform chosen, or one RomM does not know.
    #[error("{0:?} is not a RomM platform this export knows")]
    UnknownPlatform(String),
    /// Nothing to export.
    #[error("the edition has no discs with an image linked")]
    NoDiscs,
    /// A CUE sheet names a file under a name that cannot be rewritten.
    #[error("disc {disc}: the sheet's FILE line for {name:?} could not be rewritten")]
    Unrewritable {
        /// One-based disc number.
        disc: usize,
        /// The declared name.
        name: String,
    },
}

// --- naming ------------------------------------------------------------------------

/// A name safe on every filesystem RomM is likely to read from.
///
/// Characters Windows, macOS or a network share refuse become spaces or are
/// dropped, runs of spaces collapse, and a name cannot end in a dot or a
/// space, which Windows silently strips. Never empty.
#[must_use]
pub fn safe_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        let c = match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        if c == ' ' && (out.is_empty() || out.ends_with(' ')) {
            continue;
        }
        out.push(c);
    }
    let mut out: String = out.chars().take(MAX_NAME_CHARS).collect();
    while out.ends_with(' ') || out.ends_with('.') {
        out.pop();
    }
    if out.is_empty() || out.starts_with('.') {
        // A leading dot would hide the folder, and RomM would not see it.
        out.insert_str(0, "Untitled ");
        while out.ends_with(' ') || out.ends_with('.') {
            out.pop();
        }
    }
    out
}

/// The extension of a logical path, lowercased, without the dot.
fn extension(path: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty()
        || ext.is_empty()
        || ext.len() > 8
        || !ext.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

fn with_extension(base: &str, path: &str, fallback: &str) -> String {
    format!(
        "{base}.{}",
        extension(path).unwrap_or_else(|| fallback.to_owned())
    )
}

/// Plan one edition's export.
///
/// The folder is the game's name and region, `Title (Region)`, which is how
/// RomM's own naming conventions carry a region. One disc is named for the
/// game; several are `Title (Region) (Disc N)`. A CUE sheet's files are named
/// after their disc, numbered `(Track NN)` when there is more than one, in
/// the order the sheet names them, which for a one-file-per-track dump is the
/// track order.
///
/// # Errors
///
/// [`PlanError`] when the edition cannot be exported as it stands.
pub fn plan(request: &ExportRequest) -> Result<ExportPlan, PlanError> {
    if !is_platform(&request.platform) {
        return Err(PlanError::UnknownPlatform(request.platform.clone()));
    }
    if request.discs.is_empty() {
        return Err(PlanError::NoDiscs);
    }

    let game = match request
        .region
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(region) => safe_name(&format!("{} ({region})", request.title.trim())),
        None => safe_name(&request.title),
    };
    let several = request.discs.len() > 1;

    let mut files = Vec::new();
    for (index, disc) in request.discs.iter().enumerate() {
        let number = index + 1;
        let base = if several {
            format!("{game} (Disc {number})")
        } else {
            game.clone()
        };
        match disc {
            DiscImage::Iso(image) => files.push(PlannedFile {
                name: with_extension(&base, &image.logical_path, "iso"),
                source: FileSource::Object(image.object_path.clone()),
                sha256: image.sha256.clone(),
                length: image.length,
            }),
            DiscImage::CueBin {
                sheet,
                sheet_bytes,
                files: referenced,
            } => {
                let mut renames = BTreeMap::new();
                for (position, (declared, file)) in referenced.iter().enumerate() {
                    let name = if referenced.len() == 1 {
                        with_extension(&base, &file.logical_path, "bin")
                    } else {
                        with_extension(
                            &format!("{base} (Track {:02})", position + 1),
                            &file.logical_path,
                            "bin",
                        )
                    };
                    renames.insert(declared.clone(), name.clone());
                    files.push(PlannedFile {
                        name,
                        source: FileSource::Object(file.object_path.clone()),
                        sha256: file.sha256.clone(),
                        length: file.length,
                    });
                }
                let rewritten = rewrite_cue_files(sheet_bytes, &renames)
                    .map_err(|name| PlanError::Unrewritable { disc: number, name })?;
                let sha256 = hex::encode(Sha256::digest(&rewritten));
                files.push(PlannedFile {
                    name: with_extension(&base, &sheet.logical_path, "cue"),
                    length: rewritten.len() as u64,
                    sha256,
                    source: FileSource::Written(rewritten),
                });
            }
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(ExportPlan {
        edition_id: request.edition_id.clone(),
        platform: request.platform.clone(),
        folder: game,
        files,
    })
}

/// Rewrite the names a CUE sheet's `FILE` lines declare.
///
/// Only the name in each `FILE` line changes; every other byte of the sheet,
/// its line endings, its other commands, its comments, is kept. Returns the
/// declared name that has no rename, or that a new name cannot replace
/// safely, as the error.
///
/// # Errors
///
/// The declared name that could not be rewritten.
pub fn rewrite_cue_files(
    sheet: &[u8],
    renames: &BTreeMap<String, String>,
) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(sheet);
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        let indent_len = body.len() - body.trim_start_matches([' ', '\t', '\u{feff}']).len();
        let (indent, rest) = body.split_at(indent_len);
        let Some(after) = rest
            .get(..5)
            .filter(|word| {
                word.eq_ignore_ascii_case("FILE ") || word.eq_ignore_ascii_case("FILE\t")
            })
            .map(|_| &rest[5..])
        else {
            out.push_str(line);
            continue;
        };
        let after = after.trim_start();
        let (declared, tail) = if let Some(quoted) = after.strip_prefix('"') {
            let Some(end) = quoted.find('"') else {
                return Err(after.to_owned());
            };
            (&quoted[..end], &quoted[end + 1..])
        } else {
            match after.find([' ', '\t']) {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        let Some(new_name) = renames.get(declared) else {
            return Err(declared.to_owned());
        };
        if new_name.contains('"') || new_name.chars().any(char::is_control) {
            return Err(declared.to_owned());
        }
        out.push_str(indent);
        out.push_str("FILE \"");
        out.push_str(new_name);
        out.push('"');
        out.push_str(tail);
        out.push_str(ending);
    }
    Ok(out.into_bytes())
}

// --- the marker -------------------------------------------------------------------------

/// What a managed folder's marker says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// [`MARKER_SCHEMA`].
    pub schema: String,
    /// The edition the folder belongs to.
    pub edition_id: String,
    /// Every file Tangible wrote into it.
    pub files: Vec<MarkedFile>,
}

/// One file a marker lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkedFile {
    /// Its name.
    pub name: String,
    /// Its digest.
    pub sha256: String,
    /// Its length.
    pub length: u64,
}

/// How a file was put in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// A hard link to the stored object, sharing its bytes.
    Hardlink,
    /// A copy.
    Copy,
    /// Written fresh: a rewritten sheet.
    Written,
}

impl Method {
    /// As the export receipt records it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hardlink => "hardlink",
            // A rewritten sheet is a copy in every sense the receipt cares
            // about: bytes of its own, nothing shared with the store.
            Self::Copy | Self::Written => "copy",
        }
    }
}

/// What applying a plan did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The folder already matched the plan.
    Unchanged,
    /// The folder was written, each file by the method given.
    Written(Vec<(String, Method)>),
}

/// Why an export could not be written or removed.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The export root is missing or is not a directory.
    #[error("the export root is not a directory")]
    RootMissing,
    /// A folder of the game's name exists and is not this edition's.
    #[error("{folder} already exists and was not made by Tangible for this game; it is left alone")]
    Unmanaged {
        /// The folder, relative to the root.
        folder: String,
    },
    /// A managed folder holds files Tangible did not put there.
    #[error("{folder} holds files Tangible did not write ({files}); it is left alone")]
    ForeignFiles {
        /// The folder, relative to the root.
        folder: String,
        /// The files, comma separated.
        files: String,
    },
    /// A filesystem operation failed.
    #[error("could not {operation}: {source}")]
    Io {
        /// What was being done.
        operation: &'static str,
        /// What went wrong.
        source: std::io::Error,
    },
}

fn io(operation: &'static str) -> impl FnOnce(std::io::Error) -> ApplyError {
    move |source| ApplyError::Io { operation, source }
}

/// Read a folder's marker, if it has a well-formed one.
fn read_marker(folder: &Path) -> Option<Marker> {
    let bytes = std::fs::read(folder.join(MARKER)).ok()?;
    serde_json::from_slice::<Marker>(&bytes)
        .ok()
        .filter(|marker| marker.schema == MARKER_SCHEMA)
}

/// The names in a folder that are not the marker and not listed in it.
fn foreign_files(folder: &Path, marker: &Marker) -> Result<Vec<String>, ApplyError> {
    let mut foreign = Vec::new();
    for entry in std::fs::read_dir(folder).map_err(io("read the game's folder"))? {
        let entry = entry.map_err(io("read the game's folder"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != MARKER && !marker.files.iter().any(|file| file.name == name) {
            foreign.push(name);
        }
    }
    foreign.sort();
    Ok(foreign)
}

/// Check that `folder`, if it exists, is this edition's to replace or
/// remove, and return its marker.
fn owned(root: &Path, folder: &Path, edition_id: &str) -> Result<Option<Marker>, ApplyError> {
    let relative = folder
        .strip_prefix(root)
        .unwrap_or(folder)
        .to_string_lossy()
        .into_owned();
    match std::fs::symlink_metadata(folder) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io("inspect the game's folder")(error)),
        Ok(metadata) if !metadata.is_dir() => {
            return Err(ApplyError::Unmanaged { folder: relative });
        }
        Ok(_) => {}
    }
    let Some(marker) = read_marker(folder).filter(|marker| marker.edition_id == edition_id) else {
        return Err(ApplyError::Unmanaged { folder: relative });
    };
    let foreign = foreign_files(folder, &marker)?;
    if !foreign.is_empty() {
        return Err(ApplyError::ForeignFiles {
            folder: relative,
            files: foreign.join(", "),
        });
    }
    Ok(Some(marker))
}

/// Whether a folder already holds exactly what `marker` describes.
fn matches(folder: &Path, existing: &Marker, wanted: &Marker) -> bool {
    existing.files == wanted.files
        && wanted.files.iter().all(|file| {
            std::fs::metadata(folder.join(&file.name))
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == file.length)
        })
}

/// Put one stored object in place: a hard link when that cannot expose the
/// original to change, otherwise a copy.
///
/// A hard link shares the object's inode, so it is used only for an object
/// stored read-only, which every object is; and only where it works at all,
/// on the same filesystem. Anything else is copied, and the copy is made
/// read-only too: RomM reads these files and has no reason to change them.
fn place_object(object: &Path, destination: &Path) -> Result<Method, ApplyError> {
    let read_only = std::fs::metadata(object)
        .map_err(io("read a stored object"))?
        .permissions()
        .readonly();
    if read_only && std::fs::hard_link(object, destination).is_ok() {
        return Ok(Method::Hardlink);
    }
    std::fs::copy(object, destination).map_err(io("copy a stored object"))?;
    set_read_only(destination)?;
    Ok(Method::Copy)
}

fn set_read_only(path: &Path) -> Result<(), ApplyError> {
    let mut permissions = std::fs::metadata(path)
        .map_err(io("read an exported file"))?
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions).map_err(io("make an exported file read-only"))
}

/// Delete the files a managed folder's marker lists, the marker, and the
/// folder if that leaves it empty.
fn remove_listed(folder: &Path, marker: &Marker) -> Result<(), ApplyError> {
    for file in &marker.files {
        match std::fs::remove_file(folder.join(&file.name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io("remove an exported file")(error)),
        }
    }
    let _ = std::fs::remove_file(folder.join(MARKER));
    let _ = std::fs::remove_dir(folder);
    Ok(())
}

/// Write a plan's folder under `root`, replacing this edition's previous
/// export, and removing it from `previous` if the game's folder was renamed.
///
/// Blocking: call it off the async runtime.
///
/// # Errors
///
/// [`ApplyError`] when the folder is not this edition's to write, or the
/// filesystem fails. A failure leaves the previous export in place.
pub fn apply(
    root: &Path,
    plan: &ExportPlan,
    previous: Option<&str>,
) -> Result<Applied, ApplyError> {
    if !std::fs::metadata(root).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(ApplyError::RootMissing);
    }
    let platform_dir = root.join(&plan.platform);
    std::fs::create_dir_all(&platform_dir).map_err(io("create the platform folder"))?;
    if std::fs::symlink_metadata(&platform_dir)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(ApplyError::Unmanaged {
            folder: plan.platform.clone(),
        });
    }

    let target = platform_dir.join(&plan.folder);
    let wanted = plan.marker();
    let existing = owned(root, &target, &plan.edition_id)?;
    if let Some(existing) = &existing
        && matches(&target, existing, &wanted)
    {
        remove_previous(root, previous, &plan.relative_folder(), &plan.edition_id)?;
        return Ok(Applied::Unchanged);
    }

    // Built beside the destination, then swapped in, so a failure halfway
    // leaves whatever was there before.
    let staging = platform_dir.join(format!(".tangible-staging-{}", plan.edition_id));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(io("clear an abandoned staging folder"))?;
    }
    std::fs::create_dir(&staging).map_err(io("create a staging folder"))?;
    let built = (|| {
        let mut methods = Vec::with_capacity(plan.files.len());
        for file in &plan.files {
            let destination = staging.join(&file.name);
            let method = match &file.source {
                FileSource::Object(object) => place_object(object, &destination)?,
                FileSource::Written(bytes) => {
                    std::fs::write(&destination, bytes).map_err(io("write a rewritten sheet"))?;
                    set_read_only(&destination)?;
                    Method::Written
                }
            };
            methods.push((file.name.clone(), method));
        }
        let marker = serde_json::to_vec_pretty(&wanted).unwrap_or_default();
        std::fs::write(staging.join(MARKER), marker).map_err(io("write the export marker"))?;
        Ok(methods)
    })();
    let methods = match built {
        Ok(methods) => methods,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    if let Some(existing) = existing {
        let old = platform_dir.join(format!(".tangible-old-{}", plan.edition_id));
        if old.exists() {
            std::fs::remove_dir_all(&old).map_err(io("clear an abandoned old folder"))?;
        }
        std::fs::rename(&target, &old).map_err(io("move the previous export aside"))?;
        if let Err(error) = std::fs::rename(&staging, &target) {
            let _ = std::fs::rename(&old, &target);
            return Err(io("move the new export into place")(error));
        }
        remove_listed(&old, &existing)?;
    } else {
        std::fs::rename(&staging, &target).map_err(io("move the new export into place"))?;
    }

    remove_previous(root, previous, &plan.relative_folder(), &plan.edition_id)?;
    Ok(Applied::Written(methods))
}

/// Remove an edition's earlier folder, when it lived somewhere else.
fn remove_previous(
    root: &Path,
    previous: Option<&str>,
    current: &str,
    edition_id: &str,
) -> Result<(), ApplyError> {
    match previous {
        Some(previous) if previous != current => remove(root, previous, edition_id),
        _ => Ok(()),
    }
}

/// Remove an edition's export from `folder`, relative to `root`.
///
/// Only a folder whose marker names this edition, and only the files its
/// marker lists. A folder already gone is not an error.
///
/// # Errors
///
/// [`ApplyError`] when the folder is not this edition's, holds files
/// Tangible did not write, or cannot be removed.
pub fn remove(root: &Path, folder: &str, edition_id: &str) -> Result<(), ApplyError> {
    if folder
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ApplyError::Unmanaged {
            folder: folder.to_owned(),
        });
    }
    let path = root.join(folder);
    match owned(root, &path, edition_id)? {
        Some(marker) => remove_listed(&path, &marker),
        None => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn object(dir: &Path, name: &str, bytes: &[u8]) -> SourceFile {
        // Stored once and read-only, as the object store does.
        let path = dir.join(format!("object-{name}"));
        if !path.exists() {
            std::fs::write(&path, bytes).unwrap();
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        SourceFile {
            logical_path: name.to_owned(),
            object_path: path,
            sha256: hex::encode(Sha256::digest(bytes)),
            length: bytes.len() as u64,
        }
    }

    fn cue_disc(dir: &Path, tag: &str) -> DiscImage {
        let sheet_bytes = b"REM a comment\r\nFILE \"Track 01.bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\n    INDEX 01 00:00:00\r\nFILE \"Track 02.bin\" BINARY\r\n  TRACK 02 AUDIO\r\n    INDEX 01 00:00:00\r\n".to_vec();
        DiscImage::CueBin {
            sheet: object(dir, &format!("{tag}.cue"), &sheet_bytes),
            sheet_bytes,
            files: vec![
                (
                    "Track 01.bin".to_owned(),
                    object(
                        dir,
                        &format!("{tag}-1.bin"),
                        format!("{tag} data").as_bytes(),
                    ),
                ),
                (
                    "Track 02.bin".to_owned(),
                    object(
                        dir,
                        &format!("{tag}-2.bin"),
                        format!("{tag} audio").as_bytes(),
                    ),
                ),
            ],
        }
    }

    fn request(discs: Vec<DiscImage>) -> ExportRequest {
        ExportRequest {
            edition_id: "edition-1".to_owned(),
            title: "Example: The Game".to_owned(),
            region: Some("USA".to_owned()),
            platform: "psx".to_owned(),
            discs,
        }
    }

    #[test]
    fn names_are_safe_everywhere_and_never_empty() {
        assert_eq!(safe_name("Example: The Game?"), "Example The Game");
        assert_eq!(safe_name("  a  /  b  "), "a b");
        assert_eq!(safe_name("trailing..."), "trailing");
        assert_eq!(safe_name("..."), "Untitled");
        assert_eq!(safe_name(".hidden"), "Untitled .hidden");
        assert_eq!(safe_name("tab\there"), "tabhere");
        assert_eq!(safe_name(&"x".repeat(500)).chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn only_file_lines_change_when_a_sheet_is_rewritten() {
        let sheet = b"\xef\xbb\xbfFILE disc.bin BINARY\r\n  TRACK 01 MODE1/2352\r\nREM FILE \"not.bin\"\r\n  file \"two.bin\" BINARY\n";
        let renames = BTreeMap::from([
            ("disc.bin".to_owned(), "Game (Track 01).bin".to_owned()),
            ("two.bin".to_owned(), "Game (Track 02).bin".to_owned()),
        ]);
        let out = String::from_utf8(rewrite_cue_files(sheet, &renames).unwrap()).unwrap();
        assert_eq!(
            out,
            "\u{feff}FILE \"Game (Track 01).bin\" BINARY\r\n  TRACK 01 MODE1/2352\r\nREM FILE \"not.bin\"\r\n  FILE \"Game (Track 02).bin\" BINARY\n"
        );
        assert_eq!(
            rewrite_cue_files(b"FILE \"other.bin\" BINARY\n", &renames),
            Err("other.bin".to_owned())
        );
    }

    #[test]
    fn a_multi_disc_game_is_one_folder_with_numbered_discs() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(&request(vec![
            cue_disc(dir.path(), "a"),
            cue_disc(dir.path(), "b"),
        ]))
        .unwrap();
        assert_eq!(plan.relative_folder(), "psx/Example The Game (USA)");
        let names: Vec<&str> = plan.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Example The Game (USA) (Disc 1) (Track 01).bin",
                "Example The Game (USA) (Disc 1) (Track 02).bin",
                "Example The Game (USA) (Disc 1).cue",
                "Example The Game (USA) (Disc 2) (Track 01).bin",
                "Example The Game (USA) (Disc 2) (Track 02).bin",
                "Example The Game (USA) (Disc 2).cue",
            ]
        );
        let FileSource::Written(sheet) = &plan.files[2].source else {
            panic!("a rewritten sheet");
        };
        let sheet = String::from_utf8(sheet.clone()).unwrap();
        assert!(
            sheet.contains("FILE \"Example The Game (USA) (Disc 1) (Track 02).bin\" BINARY\r\n")
        );
        assert!(sheet.starts_with("REM a comment\r\n"));
    }

    #[test]
    fn a_single_iso_is_named_for_the_game() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(vec![DiscImage::Iso(object(dir.path(), "disc.ISO", b"iso"))]);
        req.region = None;
        let plan = plan(&req).unwrap();
        assert_eq!(plan.files[0].name, "Example The Game.iso");
    }

    #[test]
    fn an_unknown_platform_or_no_discs_cannot_be_planned() {
        let mut req = request(Vec::new());
        assert_eq!(plan(&req), Err(PlanError::NoDiscs));
        req.platform = "../etc".to_owned();
        assert!(matches!(plan(&req), Err(PlanError::UnknownPlatform(_))));
    }

    #[test]
    fn an_export_is_written_then_left_alone_then_replaced_then_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("roms");
        std::fs::create_dir(&root).unwrap();
        let sources = dir.path().join("objects");
        std::fs::create_dir(&sources).unwrap();

        let first = plan(&request(vec![cue_disc(&sources, "a")])).unwrap();
        let Applied::Written(methods) = apply(&root, &first, None).unwrap() else {
            panic!("written");
        };
        // Same filesystem and read-only objects: the data files are links;
        // the rewritten sheet has bytes of its own.
        assert!(methods.iter().any(|(_, m)| *m == Method::Hardlink));
        assert!(methods.iter().any(|(_, m)| *m == Method::Written));
        let folder = root.join("psx/Example The Game (USA)");
        assert!(folder.join(MARKER).exists());
        assert_eq!(
            std::fs::read_dir(&folder).unwrap().count(),
            first.files.len() + 1
        );

        assert_eq!(apply(&root, &first, None).unwrap(), Applied::Unchanged);

        // A second disc is linked: the folder is rebuilt and swapped in.
        let second = plan(&request(vec![
            cue_disc(&sources, "a"),
            cue_disc(&sources, "b"),
        ]))
        .unwrap();
        assert!(matches!(
            apply(&root, &second, None).unwrap(),
            Applied::Written(_)
        ));
        assert_eq!(
            std::fs::read_dir(&folder).unwrap().count(),
            second.files.len() + 1
        );
        assert!(!platform_leftovers(&root.join("psx")));

        // Renamed: the old folder goes, the new one appears.
        let mut renamed_request = request(vec![cue_disc(&sources, "a")]);
        renamed_request.region = Some("Europe".to_owned());
        let renamed = plan(&renamed_request).unwrap();
        apply(&root, &renamed, Some(&second.relative_folder())).unwrap();
        assert!(!folder.exists());
        assert!(root.join("psx/Example The Game (Europe)").exists());

        remove(&root, &renamed.relative_folder(), "edition-1").unwrap();
        assert!(!root.join("psx/Example The Game (Europe)").exists());
        // The library's objects are untouched throughout.
        assert!(sources.join("object-a.cue").exists());
    }

    fn platform_leftovers(platform: &Path) -> bool {
        std::fs::read_dir(platform).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tangible-")
        })
    }

    #[test]
    fn a_folder_tangible_did_not_make_is_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("roms");
        let theirs = root.join("psx/Example The Game (USA)");
        std::fs::create_dir_all(&theirs).unwrap();
        std::fs::write(theirs.join("their.chd"), b"theirs").unwrap();

        let plan = plan(&request(vec![cue_disc(dir.path(), "a")])).unwrap();
        assert!(matches!(
            apply(&root, &plan, None),
            Err(ApplyError::Unmanaged { .. })
        ));
        assert!(matches!(
            remove(&root, &plan.relative_folder(), "edition-1"),
            Err(ApplyError::Unmanaged { .. })
        ));
        assert_eq!(std::fs::read(theirs.join("their.chd")).unwrap(), b"theirs");
    }

    #[test]
    fn a_managed_folder_somebody_added_to_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("roms");
        std::fs::create_dir(&root).unwrap();
        let plan = plan(&request(vec![cue_disc(dir.path(), "a")])).unwrap();
        apply(&root, &plan, None).unwrap();
        let folder = root.join(plan.relative_folder());
        std::fs::write(folder.join("save.srm"), b"mine").unwrap();

        let err = remove(&root, &plan.relative_folder(), "edition-1").unwrap_err();
        assert!(matches!(err, ApplyError::ForeignFiles { ref files, .. } if files == "save.srm"));
        assert_eq!(std::fs::read(folder.join("save.srm")).unwrap(), b"mine");
    }

    #[test]
    fn another_editions_folder_is_not_this_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("roms");
        std::fs::create_dir(&root).unwrap();
        let plan = plan(&request(vec![cue_disc(dir.path(), "a")])).unwrap();
        apply(&root, &plan, None).unwrap();
        assert!(matches!(
            remove(&root, &plan.relative_folder(), "edition-2"),
            Err(ApplyError::Unmanaged { .. })
        ));
        assert!(matches!(
            remove(&root, "psx/../etc", "edition-1"),
            Err(ApplyError::Unmanaged { .. })
        ));
    }

    #[test]
    fn a_missing_root_is_refused_rather_than_created() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(&request(vec![cue_disc(dir.path(), "a")])).unwrap();
        assert!(matches!(
            apply(&dir.path().join("not-mounted"), &plan, None),
            Err(ApplyError::RootMissing)
        ));
    }

    #[test]
    fn every_platform_slug_is_a_plain_folder_name() {
        for (slug, name) in PLATFORMS {
            assert!(
                slug.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{slug}"
            );
            assert!(!name.is_empty());
        }
    }
}
