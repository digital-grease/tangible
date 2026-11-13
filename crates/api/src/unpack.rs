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
//! The checkpoint is saved only between runs, so how far an interrupted run
//! got is kept on disk, in an area of Tangible's own beside the import's
//! ([`StagingKind::Unpacks`], under the import's id). No source writes
//! there, so nothing a source staged can be taken for it:
//!
//! 1. The archive is moved into that area, as `archive`, before it is read.
//! 2. Its files are unpacked into `unpacking/` there, then moved into the
//!    import's area.
//! 3. A record, `record.json`, is written there, then the archive removed.
//!
//! A run that finds the record finishes step 3 and goes on; one that finds
//! the archive starts again from step 2, clearing whatever step 2 left; one
//! that finds neither starts from step 1.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tangible_domain::Sha256Digest;
use tangible_image::archive::{ArchiveError, ArchiveKind, ArchiveLimits};
use tangible_storage::{StagingArea, StagingKind, StagingManager};
use tokio::fs;
use tokio::io::AsyncReadExt as _;

use crate::import::{ImportCheckpoint, ImportError};

/// The archive, set aside in Tangible's area.
const ARCHIVE: &str = "archive";
/// Where it is unpacked before its files are moved into place.
const UNPACKING: &str = "unpacking";
/// What was unpacked, written before the archive is removed.
const RECORD: &str = "record.json";
/// The most of a record read: a real one is a few hundred bytes.
const RECORD_LIMIT: u64 = 64 << 10;

/// How long unpacking may take.
const DEADLINE: std::time::Duration = std::time::Duration::from_hours(6);

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

/// The record a finished unpack left, if there is one, read within bounds.
async fn read_record(state: &Path) -> Option<UnpackedArchive> {
    let file = fs::File::open(state.join(RECORD)).await.ok()?;
    let mut text = Vec::new();
    file.take(RECORD_LIMIT).read_to_end(&mut text).await.ok()?;
    serde_json::from_slice(&text).ok()
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
    staging: &StagingManager,
    area: &StagingArea,
    checkpoint: &mut ImportCheckpoint,
    max_unpacked_bytes: u64,
) -> Result<(), ImportError> {
    if checkpoint.archive.is_some() {
        return Ok(());
    }
    let earlier = staging.reopen(StagingKind::Unpacks, area.id()).await.ok();

    // Unpacked before a restart: finish removing the archive.
    if let Some(state) = &earlier
        && let Some(unpacked) = read_record(state.path()).await
    {
        remove_file(&state.path().join(ARCHIVE)).await?;
        checkpoint.archive = Some(unpacked);
        return Ok(());
    }
    let state = match earlier {
        Some(state) if is_file(&state.path().join(ARCHIVE)).await => state,
        // Never begun, or stopped before the archive was moved.
        _ => match set_aside(staging, area).await? {
            Some(state) => state,
            None => return Ok(()),
        },
    };
    let state = state.path();
    let archive = state.join(ARCHIVE);

    let (kind, unpacked) = unpack_into_place(area.path(), state, max_unpacked_bytes).await?;
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
    write_record(state, &done).await?;
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

async fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

/// Move the one staged file into Tangible's area, as `archive`, if it is an
/// archive. The area, if there is one to unpack.
async fn set_aside(
    staging: &StagingManager,
    area: &StagingArea,
) -> Result<Option<StagingArea>, ImportError> {
    let staged = area.entries().await?;
    let [only] = staged.as_slice() else {
        return Ok(None);
    };
    let path = area.resolve(only).await?;
    if detect(&path).await?.is_none() {
        return Ok(None);
    }
    let state = staging.create(StagingKind::Unpacks, area.id()).await?;
    fs::rename(&path, state.path().join(ARCHIVE))
        .await
        .map_err(staged_io(only.as_str()))?;
    Ok(Some(state))
}

/// Unpack the archive set aside in `state` and move its files to the top of
/// `root`, the import's area, after clearing whatever an interrupted run
/// left in either.
async fn unpack_into_place(
    root: &Path,
    state: &Path,
    max_unpacked_bytes: u64,
) -> Result<(ArchiveKind, tangible_image::archive::Unpacked), ImportError> {
    // The archive was the area's only file, so all there is now came from
    // an earlier run.
    clear(root, None).await?;
    clear(state, Some(ARCHIVE)).await?;
    let archive = &state.join(ARCHIVE);
    let unpacking = state.join(UNPACKING);
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
    let (from, into) = (archive.clone(), unpacking.clone());
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
async fn write_record(state: &Path, done: &UnpackedArchive) -> Result<(), ImportError> {
    let text = serde_json::to_vec(done).unwrap_or_default();
    let partial = state.join(format!("{RECORD}.part"));
    fs::write(&partial, &text)
        .await
        .map_err(staged_io(RECORD))?;
    fs::rename(&partial, state.join(RECORD))
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
async fn clear(root: &Path, keep: Option<&str>) -> Result<(), ImportError> {
    let io = |source| ImportError::ReadStaged {
        path: ".".to_owned(),
        source,
    };
    let mut entries = fs::read_dir(root).await.map_err(io)?;
    while let Some(entry) = entries.next_entry().await.map_err(io)? {
        if keep.is_some_and(|keep| entry.file_name() == keep) {
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
