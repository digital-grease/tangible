// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Unpacking an import that arrived as an archive (docs/plans/archives.md).
//!
//! When the one file staged for an import is a ZIP, 7z or tar, its files
//! are unpacked in its place and imported as if they had been staged
//! themselves; the archive is recorded in the manifest's origin as what they
//! came in, and not kept. A staged image that is not an archive is left
//! alone.
//!
//! The checkpoint is saved only between runs, so the area says how far an
//! interrupted run got, by names Tangible keeps for itself:
//!
//! 1. The archive is renamed to `.tangible-archive` before anything is read.
//! 2. Its files are unpacked into `.tangible-unpacking/`, then moved to the
//!    top of the area.
//! 3. A record, `.tangible-archive.json`, is written, then the archive
//!    removed.
//!
//! A run that finds the record finishes step 3 and goes on; one that finds
//! the archive starts again from step 2, clearing whatever step 2 left.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tangible_domain::LogicalPath;
use tangible_domain::Sha256Digest;
use tangible_image::archive::{ArchiveError, ArchiveKind, ArchiveLimits};
use tangible_storage::StagingArea;
use tokio::fs;
use tokio::io::AsyncReadExt as _;

use crate::import::{ImportCheckpoint, ImportError};

/// The archive, set aside under a name of Tangible's own.
const ARCHIVE: &str = ".tangible-archive";
/// Where it is unpacked before its files are moved into place.
const UNPACKING: &str = ".tangible-unpacking";
/// What was unpacked, written before the archive is removed.
const RECORD: &str = ".tangible-archive.json";

/// How long unpacking may take.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// What an import was unpacked from, as its checkpoint keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnpackedArchive {
    /// `zip`, `7z`, `tar`, `tar_gz` or `tar_xz`.
    pub format: String,
    /// The archive's SHA-256.
    pub sha256: Sha256Digest,
    /// Its length.
    pub length_bytes: u64,
    /// How many files came out.
    pub files: u64,
    /// What unpacking said.
    pub notes: Vec<String>,
}

/// Whether a staged path is one of Tangible's own, not the import's.
#[must_use]
pub fn is_reserved(path: &LogicalPath) -> bool {
    path.segments()
        .next()
        .is_some_and(|first| first.starts_with(".tangible-"))
}

/// A failed step in `name`.
fn staged_io(name: &str) -> impl FnOnce(std::io::Error) -> ImportError + '_ {
    move |source| ImportError::ReadStaged {
        path: name.to_owned(),
        source,
    }
}

/// Unpack the import's one staged file if it is an archive, recording it in
/// `checkpoint`. A no-op once done, and for anything that is not an archive.
///
/// # Errors
///
/// [`ImportError::Archive`] for an archive refused, or a staging failure.
pub async fn open_archive(
    area: &StagingArea,
    checkpoint: &mut ImportCheckpoint,
    max_unpacked_bytes: u64,
) -> Result<(), ImportError> {
    if checkpoint.archive.is_some() {
        return Ok(());
    }
    let root = area.path();
    let archive = root.join(ARCHIVE);

    // Unpacked before a restart: finish removing the archive.
    if let Ok(text) = fs::read_to_string(root.join(RECORD)).await
        && let Ok(unpacked) = serde_json::from_str::<UnpackedArchive>(&text)
    {
        remove_file(&archive).await?;
        checkpoint.archive = Some(unpacked);
        return Ok(());
    }
    if !set_aside(area, &archive).await? {
        return Ok(());
    }

    let (kind, unpacked) = unpack_into_place(root, &archive, max_unpacked_bytes).await?;
    let (sha256, length_bytes) = digest(&archive).await?;
    let done = UnpackedArchive {
        format: kind.as_str().to_owned(),
        sha256,
        length_bytes,
        files: unpacked.files.len() as u64,
        notes: unpacked.notes,
    };
    if done.files == 0 {
        return Err(ImportError::Archive(ArchiveError::Corrupt(
            "it holds no files".to_owned(),
        )));
    }
    write_record(root, &done).await?;
    remove_file(&archive).await?;
    tracing::info!(
        format = kind.as_str(),
        files = done.files,
        bytes = unpacked.expanded_bytes,
        "unpacked an import's archive"
    );
    checkpoint.archive = Some(done);
    Ok(())
}

/// Set the one staged file aside as `archive` if it is one, or find it set
/// aside already. Whether there is an archive to unpack.
async fn set_aside(area: &StagingArea, archive: &Path) -> Result<bool, ImportError> {
    if fs::symlink_metadata(archive).await.is_ok() {
        return Ok(true);
    }
    let staged = area.entries().await?;
    let [only] = staged.as_slice() else {
        return Ok(false);
    };
    if is_reserved(only) {
        return Ok(false);
    }
    let path = area.resolve(only).await?;
    if detect(&path).await?.is_none() {
        return Ok(false);
    }
    fs::rename(&path, archive)
        .await
        .map_err(staged_io(only.as_str()))?;
    Ok(true)
}

/// Unpack `archive` and move its files to the top of `root`, after clearing
/// whatever an interrupted run left.
async fn unpack_into_place(
    root: &Path,
    archive: &Path,
    max_unpacked_bytes: u64,
) -> Result<(ArchiveKind, tangible_image::archive::Unpacked), ImportError> {
    clear_all_but(root, ARCHIVE).await?;
    let unpacking = root.join(UNPACKING);
    fs::create_dir(&unpacking)
        .await
        .map_err(staged_io(UNPACKING))?;
    let Some(kind) = detect(archive).await? else {
        return Err(ImportError::Archive(ArchiveError::Corrupt(
            "it is no longer recognised as an archive".to_owned(),
        )));
    };
    let limits = ArchiveLimits {
        deadline: Some(std::time::Instant::now() + DEADLINE),
        ..ArchiveLimits::for_import(max_unpacked_bytes)
    };
    let (from, into) = (archive.to_path_buf(), unpacking.clone());
    let unpacked = tokio::task::spawn_blocking(move || {
        tangible_image::archive::unpack(&from, kind, &into, &limits)
    })
    .await
    .map_err(|_| ImportError::Archive(ArchiveError::TimedOut))?
    .map_err(ImportError::Archive)?;

    // Into place, as if staged as they are.
    let mut moved = fs::read_dir(&unpacking)
        .await
        .map_err(staged_io(UNPACKING))?;
    while let Some(entry) = moved.next_entry().await.map_err(staged_io(UNPACKING))? {
        let name = entry.file_name().to_string_lossy().into_owned();
        fs::rename(entry.path(), root.join(entry.file_name()))
            .await
            .map_err(staged_io(&name))?;
    }
    fs::remove_dir(&unpacking)
        .await
        .map_err(staged_io(UNPACKING))?;
    Ok((kind, unpacked))
}

/// Write the record, whole or not at all.
async fn write_record(root: &Path, done: &UnpackedArchive) -> Result<(), ImportError> {
    let text = serde_json::to_vec(done).unwrap_or_default();
    let partial = root.join(format!("{RECORD}.part"));
    fs::write(&partial, &text)
        .await
        .map_err(staged_io(RECORD))?;
    fs::rename(&partial, root.join(RECORD))
        .await
        .map_err(staged_io(RECORD))
}

async fn detect(path: &Path) -> Result<Option<ArchiveKind>, ImportError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || tangible_image::archive::detect(&path))
        .await
        .map_err(|_| ImportError::Archive(ArchiveError::TimedOut))?
        .map_err(ImportError::Archive)
}

async fn remove_file(path: &Path) -> Result<(), ImportError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ImportError::ReadStaged {
            path: ARCHIVE.to_owned(),
            source,
        }),
    }
}

/// Remove everything at the top of `root` but `keep`.
async fn clear_all_but(root: &Path, keep: &str) -> Result<(), ImportError> {
    let io = |source| ImportError::ReadStaged {
        path: ".".to_owned(),
        source,
    };
    let mut entries = fs::read_dir(root).await.map_err(io)?;
    while let Some(entry) = entries.next_entry().await.map_err(io)? {
        if entry.file_name() == keep {
            continue;
        }
        // Not followed: a link here would be removed, not what it names.
        let kind = entry.file_type().await.map_err(io)?;
        if kind.is_dir() {
            fs::remove_dir_all(entry.path()).await.map_err(io)?;
        } else {
            fs::remove_file(entry.path()).await.map_err(io)?;
        }
    }
    Ok(())
}

async fn digest(path: &Path) -> Result<(Sha256Digest, u64), ImportError> {
    let io = |source| ImportError::ReadStaged {
        path: ARCHIVE.to_owned(),
        source,
    };
    let mut file = fs::File::open(path).await.map_err(io)?;
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let mut chunk = vec![0_u8; 1 << 20];
    loop {
        let read = file.read(&mut chunk).await.map_err(io)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
        length += read as u64;
    }
    let bytes: [u8; 32] = hasher.finalize().into();
    Ok((Sha256Digest::from_bytes(bytes), length))
}
