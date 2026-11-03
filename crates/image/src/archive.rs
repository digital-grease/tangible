// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Unpacking an archive an import arrived in (docs/plans/archives.md).
//!
//! ZIP, 7z and tar (plain, gzip and xz), the owner's choice of 2026-11-02.
//! An archive is how the bytes travelled, not what is kept: its files are
//! unpacked into an empty directory and imported as they are, and the archive
//! is recorded as where they came from.
//!
//! Everything in an archive is untrusted (SECURITY.md 3.1 and 3.2), so:
//!
//! - Recognised by signature, never by name.
//! - Every entry's name must be a [`LogicalPath`]: relative, no `..`, no
//!   device names, no backslashes, bounded in length. Nothing is renamed to
//!   make it fit; an archive with such a name is refused.
//! - Only files and folders. A link of either kind, a device, a pipe or a
//!   sparse file refuses the archive.
//! - Bounded: the number of entries, the bytes unpacked, the ratio of those
//!   to the archive's size, a 7z dictionary, and the time taken.
//! - Encrypted archives are refused: Tangible holds no passwords for them.
//! - Nothing is overwritten: a name twice refuses a tar or a 7z, and the
//!   names Tangible keeps for itself (`.tangible-…`) are refused. A ZIP's
//!   directory is read one entry per name, as ZIP readers read it, so a name
//!   twice there gives one file.
//! - An archive inside the archive is unpacked as a file, not opened.
//!
//! Synchronous: decompression is CPU work. Call it from a blocking task.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

use tangible_domain::LogicalPath;

/// The archive formats unpacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// PKZIP, stored and deflate entries.
    Zip,
    /// 7-Zip.
    SevenZip,
    /// A POSIX or GNU tar.
    Tar,
    /// A gzip-compressed tar.
    TarGz,
    /// An xz-compressed tar.
    TarXz,
}

impl ArchiveKind {
    /// A stable name, as manifests and checkpoints record it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::SevenZip => "7z",
            Self::Tar => "tar",
            Self::TarGz => "tar_gz",
            Self::TarXz => "tar_xz",
        }
    }

    /// The kind with this name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [
            Self::Zip,
            Self::SevenZip,
            Self::Tar,
            Self::TarGz,
            Self::TarXz,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == name)
    }
}

/// How far an archive may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    /// Entries, folders included.
    pub max_entries: u64,
    /// Bytes unpacked, in all.
    pub max_expanded_bytes: u64,
    /// Bytes unpacked per byte of archive, past [`Self::ratio_floor_bytes`].
    pub max_ratio: u64,
    /// What may always be unpacked, however small the archive: a few
    /// megabytes of zeros compress to nothing, and are no bomb.
    pub ratio_floor_bytes: u64,
    /// The largest LZMA dictionary a 7z may ask for: the decoder allocates
    /// it before reading a byte.
    pub max_dictionary_bytes: u64,
    /// When to give up.
    pub deadline: Option<Instant>,
}

impl ArchiveLimits {
    /// The limits for an import that may unpack `max_expanded_bytes`.
    #[must_use]
    pub fn for_import(max_expanded_bytes: u64) -> Self {
        Self {
            max_entries: 10_000,
            max_expanded_bytes,
            max_ratio: 1_000,
            ratio_floor_bytes: 64 << 20,
            max_dictionary_bytes: 1 << 30,
            deadline: None,
        }
    }
}

/// What was unpacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    /// The archive's kind.
    pub kind: ArchiveKind,
    /// Every file, with its length, in the archive's order.
    pub files: Vec<(LogicalPath, u64)>,
    /// Bytes unpacked.
    pub expanded_bytes: u64,
    /// Things worth saying: what was left out, archives inside it.
    pub notes: Vec<String>,
}

/// Why an archive was not unpacked.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// It could not be read as what it claims to be.
    #[error("the archive could not be read: {0}")]
    Corrupt(String),
    /// It, or something in it, is encrypted.
    #[error("the archive is encrypted, and Tangible opens no encrypted archive")]
    Encrypted,
    /// A compression method or feature not read here.
    #[error("the archive uses {0}, which Tangible does not unpack")]
    Unsupported(String),
    /// An entry's name is not one Tangible will write.
    #[error("the archive names {name:?}, which is not a path Tangible unpacks: {why}")]
    Path {
        /// The name, as the archive spells it.
        name: String,
        /// Why not.
        why: String,
    },
    /// Something other than a file or a folder.
    #[error("the archive holds {name:?}, a {kind}; only files and folders are unpacked")]
    NotAFile {
        /// The entry.
        name: String,
        /// What it is.
        kind: &'static str,
    },
    /// The same name twice.
    #[error("the archive names {0:?} more than once")]
    Duplicate(String),
    /// Too many entries.
    #[error("the archive holds more than {0} entries")]
    TooManyEntries(u64),
    /// Too many bytes.
    #[error("the archive unpacks to more than {0} bytes")]
    TooLarge(u64),
    /// Too many bytes for its size.
    #[error("the archive unpacks to more than {0} times its own size")]
    Ratio(u64),
    /// A dictionary too large to allocate.
    #[error("the archive's dictionary needs {needed} bytes, more than the {limit} allowed")]
    Dictionary {
        /// What it asks for.
        needed: u64,
        /// What is allowed.
        limit: u64,
    },
    /// The deadline passed.
    #[error("unpacking the archive took longer than allowed")]
    TimedOut,
    /// The disk refused.
    #[error("{operation} failed")]
    Io {
        /// What was attempted.
        operation: &'static str,
        /// Why.
        #[source]
        source: io::Error,
    },
}

impl ArchiveError {
    /// A stable code, as an import's failure records it.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Corrupt(_) => "ARCHIVE_CORRUPT",
            Self::Encrypted => "ARCHIVE_ENCRYPTED",
            Self::Unsupported(_) => "ARCHIVE_UNSUPPORTED",
            Self::Path { .. } | Self::NotAFile { .. } | Self::Duplicate(_) => {
                "ARCHIVE_UNSAFE_ENTRY"
            }
            Self::TooManyEntries(_)
            | Self::TooLarge(_)
            | Self::Ratio(_)
            | Self::Dictionary { .. } => "ARCHIVE_TOO_LARGE",
            Self::TimedOut => "ARCHIVE_TIMED_OUT",
            Self::Io { .. } => "ARCHIVE_IO",
        }
    }
}

fn io(operation: &'static str) -> impl FnOnce(io::Error) -> ArchiveError {
    move |source| ArchiveError::Io { operation, source }
}

const ZIP_SIGNATURES: [&[u8]; 3] = [b"PK\x03\x04", b"PK\x05\x06", b"PK\x07\x08"];
const SEVEN_ZIP_SIGNATURE: &[u8] = &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
const GZIP_SIGNATURE: &[u8] = &[0x1F, 0x8B];
const XZ_SIGNATURE: &[u8] = &[0xFD, b'7', b'z', b'X', b'Z', 0x00];
/// Where a tar header says `ustar`, POSIX's or GNU's.
const USTAR_OFFSET: usize = 257;

/// Whether `block`, a tar's first 512 bytes, is a tar header.
fn is_tar_header(block: &[u8]) -> bool {
    block.get(USTAR_OFFSET..USTAR_OFFSET + 5) == Some(b"ustar")
}

/// Recognise an archive by its first bytes, reading at most a block of a
/// compressed tar to see a tar header inside.
///
/// # Errors
///
/// [`ArchiveError::Io`] when the file cannot be read.
pub fn detect(path: &Path) -> Result<Option<ArchiveKind>, ArchiveError> {
    let mut file = fs::File::open(path).map_err(io("opening the archive"))?;
    let mut head = [0_u8; 512];
    let read = read_up_to(&mut file, &mut head).map_err(io("reading the archive"))?;
    let head = &head[..read];
    if ZIP_SIGNATURES
        .iter()
        .any(|signature| head.starts_with(signature))
    {
        return Ok(Some(ArchiveKind::Zip));
    }
    if head.starts_with(SEVEN_ZIP_SIGNATURE) {
        return Ok(Some(ArchiveKind::SevenZip));
    }
    if is_tar_header(head) {
        return Ok(Some(ArchiveKind::Tar));
    }
    // A compressed file is a tar only if a tar header comes out of it.
    let compressed = if head.starts_with(GZIP_SIGNATURE) {
        Some(ArchiveKind::TarGz)
    } else if head.starts_with(XZ_SIGNATURE) {
        Some(ArchiveKind::TarXz)
    } else {
        None
    };
    let Some(kind) = compressed else {
        return Ok(None);
    };
    file.seek(SeekFrom::Start(0))
        .map_err(io("reading the archive"))?;
    let mut block = [0_u8; 512];
    let inner = match kind {
        ArchiveKind::TarGz => read_up_to(&mut flate2::read::GzDecoder::new(&mut file), &mut block),
        _ => read_up_to(&mut lzma_rust2::XzReader::new(&mut file, false), &mut block),
    };
    // A compressed file that will not decompress is not an archive here; it
    // is imported as the file it is.
    Ok(inner
        .ok()
        .filter(|&read| is_tar_header(&block[..read]))
        .map(|_| kind))
}

/// Fill `buffer` as far as the reader goes.
fn read_up_to(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

/// Unpack the archive at `archive` into `destination`, a directory that
/// must be empty, within `limits`.
///
/// On an error, whatever was written stays in `destination` for the caller
/// to remove.
///
/// # Errors
///
/// [`ArchiveError`]: what made it unsafe, too large, unreadable or slow.
pub fn unpack(
    archive: &Path,
    kind: ArchiveKind,
    destination: &Path,
    limits: &ArchiveLimits,
) -> Result<Unpacked, ArchiveError> {
    let archive_bytes = fs::metadata(archive)
        .map_err(io("reading the archive"))?
        .len();
    let mut writer = Writer::new(kind, destination, limits, archive_bytes)?;
    let file = fs::File::open(archive).map_err(io("opening the archive"))?;
    match kind {
        ArchiveKind::Zip => unpack_zip(file, &mut writer)?,
        ArchiveKind::SevenZip => unpack_seven_zip(file, &mut writer)?,
        ArchiveKind::Tar => unpack_tar(file, &mut writer)?,
        ArchiveKind::TarGz => unpack_tar(flate2::read::GzDecoder::new(file), &mut writer)?,
        ArchiveKind::TarXz => unpack_tar(lzma_rust2::XzReader::new(file, false), &mut writer)?,
    }
    Ok(writer.finish())
}

/// What every format's entries are written through: the names checked, the
/// limits counted, nothing overwritten.
struct Writer<'a> {
    kind: ArchiveKind,
    destination: &'a Path,
    limits: &'a ArchiveLimits,
    /// What may be unpacked: the limit, or less for a small archive.
    budget: u64,
    entries: u64,
    expanded: u64,
    files: Vec<(LogicalPath, u64)>,
    notes: Vec<String>,
}

impl<'a> Writer<'a> {
    fn new(
        kind: ArchiveKind,
        destination: &'a Path,
        limits: &'a ArchiveLimits,
        archive_bytes: u64,
    ) -> Result<Self, ArchiveError> {
        let mut listing = fs::read_dir(destination).map_err(io("opening the destination"))?;
        if listing.next().is_some() {
            return Err(ArchiveError::Io {
                operation: "unpacking into a directory that is not empty",
                source: io::Error::from(io::ErrorKind::AlreadyExists),
            });
        }
        let by_ratio = archive_bytes
            .saturating_mul(limits.max_ratio)
            .max(limits.ratio_floor_bytes);
        Ok(Self {
            kind,
            destination,
            limits,
            budget: by_ratio.min(limits.max_expanded_bytes),
            entries: 0,
            expanded: 0,
            files: Vec::new(),
            notes: Vec::new(),
        })
    }

    fn check_time(&self) -> Result<(), ArchiveError> {
        if self
            .limits
            .deadline
            .is_some_and(|deadline| Instant::now() > deadline)
        {
            return Err(ArchiveError::TimedOut);
        }
        Ok(())
    }

    /// Count one entry, and say whether it is kept: `None` for what is left
    /// out, such as macOS's resource forks.
    fn admit(&mut self, raw: &str, is_dir: bool) -> Result<Option<LogicalPath>, ArchiveError> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(ArchiveError::TooManyEntries(self.limits.max_entries));
        }
        self.check_time()?;
        let name = raw.trim_end_matches('/');
        if name.is_empty() && is_dir {
            return Ok(None);
        }
        let path = LogicalPath::parse(name).map_err(|error| ArchiveError::Path {
            name: raw.to_owned(),
            why: error.to_string(),
        })?;
        let segments: Vec<&str> = path.segments().collect();
        if segments
            .iter()
            .any(|segment| segment.starts_with(".tangible-"))
        {
            return Err(ArchiveError::Path {
                name: raw.to_owned(),
                why: "names beginning .tangible- are Tangible's own".to_owned(),
            });
        }
        if segments.first() == Some(&"__MACOSX") || segments.last() == Some(&".DS_Store") {
            if !self.notes.iter().any(|note| note.starts_with("macOS")) {
                self.notes
                    .push("macOS's own files (__MACOSX, .DS_Store) were left out".to_owned());
            }
            return Ok(None);
        }
        Ok(Some(path))
    }

    fn folder(&mut self, raw: &str) -> Result<(), ArchiveError> {
        if let Some(path) = self.admit(raw, true)? {
            fs::create_dir_all(self.destination.join(path.as_str()))
                .map_err(io("creating a folder from the archive"))?;
        }
        Ok(())
    }

    /// Write one file's bytes from `reader`.
    fn file(&mut self, raw: &str, reader: &mut dyn Read) -> Result<(), ArchiveError> {
        let Some(path) = self.admit(raw, false)? else {
            // Read past what is left out, so a stream stays in step.
            io::copy(reader, &mut io::sink()).map_err(|error| corrupt(&error))?;
            return Ok(());
        };
        let target = self.destination.join(path.as_str());
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io("creating a folder from the archive"))?;
        }
        let mut out = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(out) => out,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(ArchiveError::Duplicate(raw.to_owned()));
            }
            Err(error) => return Err(io("writing a file from the archive")(error)),
        };
        let mut chunk = vec![0_u8; 1 << 20];
        let mut length = 0_u64;
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(corrupt(&error)),
            };
            let read_u64 = read as u64;
            length += read_u64;
            self.expanded += read_u64;
            if self.expanded > self.budget {
                return Err(if self.budget < self.limits.max_expanded_bytes {
                    ArchiveError::Ratio(self.limits.max_ratio)
                } else {
                    ArchiveError::TooLarge(self.limits.max_expanded_bytes)
                });
            }
            out.write_all(&chunk[..read])
                .map_err(io("writing a file from the archive"))?;
            self.check_time()?;
        }
        out.sync_all()
            .map_err(io("flushing a file from the archive"))?;
        if is_archive_name(path.as_str()) {
            self.notes.push(format!(
                "{} looks like an archive itself; it was imported as a file, not opened",
                path.as_str()
            ));
        }
        self.files.push((path, length));
        Ok(())
    }

    fn finish(self) -> Unpacked {
        Unpacked {
            kind: self.kind,
            files: self.files,
            expanded_bytes: self.expanded,
            notes: self.notes,
        }
    }
}

/// A read error from inside an archive's data: the archive, not the disk.
fn corrupt(error: &io::Error) -> ArchiveError {
    ArchiveError::Corrupt(error.to_string())
}

/// Whether a name looks like an archive's, for the note that one inside
/// another is not opened.
fn is_archive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".zip", ".7z", ".tar", ".tgz", ".gz", ".xz", ".rar"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn unpack_zip(file: fs::File, writer: &mut Writer<'_>) -> Result<(), ArchiveError> {
    let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;
    let count = archive.len();
    if count as u64 > writer.limits.max_entries {
        return Err(ArchiveError::TooManyEntries(writer.limits.max_entries));
    }
    for index in 0..count {
        // Its metadata first, raw, so an encrypted or linked entry is refused
        // before anything decrypts or decompresses it.
        let (name, is_dir, is_symlink, encrypted) = {
            let raw = archive.by_index_raw(index).map_err(zip_error)?;
            let name = raw.name().map_err(zip_error)?.into_owned();
            (name, raw.is_dir(), raw.is_symlink(), raw.encrypted())
        };
        if encrypted {
            return Err(ArchiveError::Encrypted);
        }
        if is_symlink {
            return Err(ArchiveError::NotAFile {
                name,
                kind: "symbolic link",
            });
        }
        if is_dir {
            writer.folder(&name)?;
            continue;
        }
        let mut entry = archive.by_index(index).map_err(zip_error)?;
        writer.file(&name, &mut entry)?;
    }
    Ok(())
}

fn zip_error(error: zip::result::ZipError) -> ArchiveError {
    match error {
        zip::result::ZipError::UnsupportedArchive(what) => {
            if what.to_ascii_lowercase().contains("password")
                || what.to_ascii_lowercase().contains("encrypt")
            {
                ArchiveError::Encrypted
            } else {
                ArchiveError::Unsupported(what.to_string())
            }
        }
        zip::result::ZipError::Io(error) => corrupt(&error),
        other => ArchiveError::Corrupt(other.to_string()),
    }
}

/// The dictionary an LZMA or LZMA2 coder asks for, from its properties.
fn dictionary_bytes(method: &[u8], properties: &[u8]) -> Option<u64> {
    if method == sevenz_rust2::EncoderMethod::ID_LZMA {
        let bytes: [u8; 4] = properties.get(1..5)?.try_into().ok()?;
        Some(u64::from(u32::from_le_bytes(bytes)))
    } else if method == sevenz_rust2::EncoderMethod::ID_LZMA2 {
        let d = u32::from(*properties.first()?);
        if d > 40 {
            return Some(u64::MAX);
        }
        if d == 40 {
            return Some(u64::from(u32::MAX));
        }
        Some(u64::from((2 | (d & 1)) << (d / 2 + 11)))
    } else {
        None
    }
}

fn unpack_seven_zip(file: fs::File, writer: &mut Writer<'_>) -> Result<(), ArchiveError> {
    let mut reader = sevenz_rust2::ArchiveReader::new(file, sevenz_rust2::Password::empty())
        .map_err(|error| seven_zip_error(&error))?;
    let archive = reader.archive();
    if archive.files.len() as u64 > writer.limits.max_entries {
        return Err(ArchiveError::TooManyEntries(writer.limits.max_entries));
    }
    // Before decoding anything: what it would allocate, and whether it is
    // encrypted.
    for block in &archive.blocks {
        for coder in &block.coders {
            let method = coder.encoder_method_id();
            if method == sevenz_rust2::EncoderMethod::ID_AES256_SHA256 {
                return Err(ArchiveError::Encrypted);
            }
            if let Some(needed) = dictionary_bytes(method, coder.properties())
                && needed > writer.limits.max_dictionary_bytes
            {
                return Err(ArchiveError::Dictionary {
                    needed,
                    limit: writer.limits.max_dictionary_bytes,
                });
            }
        }
    }
    for entry in &archive.files {
        if entry.is_anti_item {
            return Err(ArchiveError::Unsupported(
                "anti-items (a 7z update)".to_owned(),
            ));
        }
        // Windows' reparse-point attribute marks a link.
        if entry.has_windows_attributes && entry.windows_attributes & 0x400 != 0 {
            return Err(ArchiveError::NotAFile {
                name: entry.name.clone(),
                kind: "link",
            });
        }
    }
    let mut failure: Option<ArchiveError> = None;
    reader
        .for_each_entries(|entry, data| {
            let outcome = if entry.is_directory {
                writer.folder(&entry.name)
            } else {
                writer.file(&entry.name, data)
            };
            match outcome {
                Ok(()) => Ok(true),
                Err(error) => {
                    failure = Some(error);
                    Ok(false)
                }
            }
        })
        .map_err(|error| seven_zip_error(&error))?;
    failure.map_or(Ok(()), Err)
}

fn seven_zip_error(error: &sevenz_rust2::Error) -> ArchiveError {
    let text = error.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("password") || lower.contains("encrypt") || lower.contains("aes") {
        ArchiveError::Encrypted
    } else if lower.contains("unsupported") {
        ArchiveError::Unsupported(text)
    } else {
        ArchiveError::Corrupt(text)
    }
}

fn unpack_tar(reader: impl Read, writer: &mut Writer<'_>) -> Result<(), ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|error| corrupt(&error))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| corrupt(&error))?;
        let name = {
            let path = entry.path().map_err(|error| corrupt(&error))?;
            path.to_str()
                .ok_or_else(|| ArchiveError::Path {
                    name: path.to_string_lossy().into_owned(),
                    why: "it is not UTF-8".to_owned(),
                })?
                .to_owned()
        };
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            writer.folder(&name)?;
        } else if kind.is_file() || kind == tar::EntryType::Continuous {
            writer.file(&name, &mut entry)?;
        } else if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            // Metadata the reader has already applied; nothing to write.
        } else {
            return Err(ArchiveError::NotAFile {
                name,
                kind: if kind.is_symlink() {
                    "symbolic link"
                } else if kind.is_hard_link() {
                    "hard link"
                } else if kind.is_gnu_sparse() {
                    "sparse file"
                } else {
                    "device or pipe"
                },
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_their_names() {
        for kind in [
            ArchiveKind::Zip,
            ArchiveKind::SevenZip,
            ArchiveKind::Tar,
            ArchiveKind::TarGz,
            ArchiveKind::TarXz,
        ] {
            assert_eq!(ArchiveKind::from_name(kind.as_str()), Some(kind));
        }
        assert_eq!(ArchiveKind::from_name("rar"), None);
    }

    #[test]
    fn lzma2_dictionaries_are_read_as_7_zip_writes_them() {
        let lzma2 = sevenz_rust2::EncoderMethod::ID_LZMA2;
        // (2 | (d & 1)) << (d / 2 + 11): 0 is 4 KiB, 0x18 is 16 MiB, 0x1C
        // 64 MiB, and an odd value adds half again (0x17 is 12 MiB).
        assert_eq!(dictionary_bytes(lzma2, &[0]), Some(4 << 10));
        assert_eq!(dictionary_bytes(lzma2, &[0x18]), Some(16 << 20));
        assert_eq!(dictionary_bytes(lzma2, &[0x1C]), Some(64 << 20));
        assert_eq!(dictionary_bytes(lzma2, &[0x17]), Some(12 << 20));
        assert_eq!(dictionary_bytes(lzma2, &[40]), Some(u64::from(u32::MAX)));
        assert_eq!(dictionary_bytes(lzma2, &[41]), Some(u64::MAX));
        let lzma = sevenz_rust2::EncoderMethod::ID_LZMA;
        let mut properties = vec![0x5D];
        properties.extend_from_slice(&(1_u32 << 24).to_le_bytes());
        assert_eq!(dictionary_bytes(lzma, &properties), Some(1 << 24));
        assert_eq!(dictionary_bytes(&[0x00], &[]), None);
    }
}
