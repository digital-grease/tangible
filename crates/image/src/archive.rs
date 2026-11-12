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
//! - Nothing is overwritten: a name twice refuses a tar or a 7z. A ZIP's
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
    /// The archive's own index is larger than any real one: what the
    /// decoder would read whole before a single entry could be counted.
    #[error("the archive's {0} is larger than Tangible reads")]
    Index(&'static str),
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
            | Self::Index(_)
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

/// The largest index read whole before entries are counted: a 7z's header
/// or a ZIP's central directory. Ten thousand entries with the longest names
/// need about 11 MB.
const MAX_INDEX_BYTES: u64 = 64 << 20;

/// The largest tar extension header (a GNU long name or link, a PAX record):
/// the tar reader holds each whole in memory.
const MAX_TAR_EXTENSION_BYTES: u64 = 1 << 20;

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
    let mut file = fs::File::open(archive).map_err(io("opening the archive"))?;
    // What the decoders would read whole into memory before a single entry
    // reaches a limit is checked first, from the bytes.
    match kind {
        ArchiveKind::Zip => zip_index_fits(&mut file, archive_bytes, limits)?,
        ArchiveKind::SevenZip => seven_zip_index_fits(&mut file, archive_bytes, limits)?,
        _ => {}
    }
    file.seek(SeekFrom::Start(0))
        .map_err(io("reading the archive"))?;
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
        // `tar -C dir -cf x.tar .` names everything under `./`.
        let mut name = raw.trim_end_matches('/');
        while let Some(rest) = name.strip_prefix("./") {
            name = rest;
        }
        if (name.is_empty() || name == ".") && is_dir {
            return Ok(None);
        }
        let path = LogicalPath::parse(name).map_err(|error| ArchiveError::Path {
            name: raw.to_owned(),
            why: error.to_string(),
        })?;
        let segments: Vec<&str> = path.segments().collect();
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
            make_folder(&self.destination.join(path.as_str()), raw)?;
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
            make_folder(parent, raw)?;
        }
        let mut out = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(out) => out,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::NotADirectory
                ) =>
            {
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

/// Create a folder the archive needs. A file already where a folder goes, or
/// the other way about, is the archive naming one thing twice, and is final:
/// it would fail the same way every time.
fn make_folder(path: &Path, raw: &str) -> Result<(), ArchiveError> {
    match fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::AlreadyExists | io::ErrorKind::NotADirectory
            ) =>
        {
            Err(ArchiveError::Duplicate(raw.to_owned()))
        }
        Err(error) => Err(io("creating a folder from the archive")(error)),
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

/// Read `buffer.len()` bytes at `offset`, or fail as a damaged archive.
fn read_at(file: &mut fs::File, offset: u64, buffer: &mut [u8]) -> Result<(), ArchiveError> {
    file.seek(SeekFrom::Start(offset))
        .map_err(io("reading the archive"))?;
    file.read_exact(buffer)
        .map_err(|_| ArchiveError::Corrupt("it ends early".to_owned()))
}

fn le_u16(bytes: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

fn le_u32(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
    ]))
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0_u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

/// A ZIP's entry count and central directory size, from its end record (and
/// the ZIP64 one where that says so), checked before the reader builds its
/// index of every entry in memory.
fn zip_index_fits(
    file: &mut fs::File,
    length: u64,
    limits: &ArchiveLimits,
) -> Result<(), ArchiveError> {
    // The end record is 22 bytes, with up to 64 KiB of comment after it.
    let tail_length = length.min(22 + 65_535);
    let mut tail = vec![0_u8; usize::try_from(tail_length).unwrap_or(0)];
    read_at(file, length - tail_length, &mut tail)?;
    let Some(at) = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&at| tail[at..at + 4] == *b"PK\x05\x06")
    else {
        return Err(ArchiveError::Corrupt("it has no ZIP end record".to_owned()));
    };
    let mut entries = le_u16(&tail, at + 10);
    let mut size = le_u32(&tail, at + 12);
    if entries == 0xFFFF || size == 0xFFFF_FFFF || le_u32(&tail, at + 16) == 0xFFFF_FFFF {
        // ZIP64: a locator just before the end record names the real one.
        let locator = (length - tail_length + at as u64)
            .checked_sub(20)
            .ok_or_else(|| ArchiveError::Corrupt("its ZIP64 locator is missing".to_owned()))?;
        let mut bytes = [0_u8; 20];
        read_at(file, locator, &mut bytes)?;
        if bytes[..4] != *b"PK\x06\x07" {
            return Err(ArchiveError::Corrupt(
                "its ZIP64 locator is missing".to_owned(),
            ));
        }
        let mut record = [0_u8; 56];
        read_at(file, le_u64(&bytes, 8), &mut record)?;
        if record[..4] != *b"PK\x06\x06" {
            return Err(ArchiveError::Corrupt(
                "its ZIP64 end record is missing".to_owned(),
            ));
        }
        entries = le_u64(&record, 32);
        size = le_u64(&record, 40);
    }
    if entries > limits.max_entries {
        return Err(ArchiveError::TooManyEntries(limits.max_entries));
    }
    if size > MAX_INDEX_BYTES {
        return Err(ArchiveError::Index("central directory"));
    }
    Ok(())
}

/// Reads a 7z header's fields, each read bounded by the bytes there are.
struct HeaderCursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

fn header_unreadable() -> ArchiveError {
    ArchiveError::Corrupt("its header could not be read".to_owned())
}

impl<'a> HeaderCursor<'a> {
    fn byte(&mut self) -> Result<u8, ArchiveError> {
        let value = *self.bytes.get(self.at).ok_or_else(header_unreadable)?;
        self.at += 1;
        Ok(value)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ArchiveError> {
        let slice = self
            .bytes
            .get(self.at..self.at.checked_add(length).ok_or_else(header_unreadable)?)
            .ok_or_else(header_unreadable)?;
        self.at += length;
        Ok(slice)
    }

    /// A 7z number: one to nine bytes, the first saying how many follow.
    fn number(&mut self) -> Result<u64, ArchiveError> {
        let first = self.byte()?;
        let mut mask = 0x80_u8;
        let mut value = 0_u64;
        for i in 0..8 {
            if first & mask == 0 {
                let high = u64::from(first & mask.wrapping_sub(1));
                return Ok(value | (high << (8 * i)));
            }
            value |= u64::from(self.byte()?) << (8 * i);
            mask >>= 1;
        }
        Ok(value)
    }

    /// A number that counts something, bounded so a hostile header cannot
    /// make the reading of it loop for long.
    fn count(&mut self) -> Result<u64, ArchiveError> {
        let value = self.number()?;
        if value > 1024 {
            return Err(header_unreadable());
        }
        Ok(value)
    }
}

/// What a 7z's encoded header will decode to, and the dictionaries its own
/// coders ask for, read from the bytes before the reader decodes it whole
/// into memory. A plain header is read as it is, so its size on disk bounds
/// it.
fn seven_zip_index_fits(
    file: &mut fs::File,
    length: u64,
    limits: &ArchiveLimits,
) -> Result<(), ArchiveError> {
    let mut start = [0_u8; 32];
    read_at(file, 0, &mut start)?;
    let offset = le_u64(&start, 12);
    let size = le_u64(&start, 20);
    if size > MAX_INDEX_BYTES {
        return Err(ArchiveError::Index("header"));
    }
    let at = 32_u64
        .checked_add(offset)
        .filter(|at| at.checked_add(size).is_some_and(|end| end <= length))
        .ok_or_else(header_unreadable)?;
    let mut header = vec![0_u8; usize::try_from(size).map_err(|_| header_unreadable())?];
    read_at(file, at, &mut header)?;
    // kHeader is plain. Anything else but kEncodedHeader is the reader's to
    // refuse.
    if header.first() != Some(&0x17) {
        return Ok(());
    }
    let mut cursor = HeaderCursor {
        bytes: &header,
        at: 1,
    };
    loop {
        match cursor.byte()? {
            0x00 => return Ok(()),
            // PackInfo: where the packed header is, and its sizes.
            0x06 => {
                cursor.number()?;
                let streams = cursor.count()?;
                loop {
                    match cursor.byte()? {
                        0x00 => break,
                        0x09 => {
                            for _ in 0..streams {
                                cursor.number()?;
                            }
                        }
                        _ => return Err(header_unreadable()),
                    }
                }
            }
            // UnpackInfo: the coders, and what each folder decodes to. What
            // follows (CRCs, the end) the reader checks.
            0x07 => return seven_zip_unpack_info(&mut cursor, limits),
            _ => return Err(header_unreadable()),
        }
    }
}

/// An encoded header's UnpackInfo: each folder's coders checked, and every
/// size it decodes to.
fn seven_zip_unpack_info(
    cursor: &mut HeaderCursor<'_>,
    limits: &ArchiveLimits,
) -> Result<(), ArchiveError> {
    if cursor.byte()? != 0x0B {
        return Err(header_unreadable());
    }
    let folders = cursor.count()?;
    if cursor.byte()? != 0 {
        return Err(header_unreadable());
    }
    let mut outputs = 0_u64;
    for _ in 0..folders {
        outputs += seven_zip_folder(cursor, limits)?;
    }
    if cursor.byte()? != 0x0C {
        return Err(header_unreadable());
    }
    for _ in 0..outputs {
        if cursor.number()? > MAX_INDEX_BYTES {
            return Err(ArchiveError::Index("header"));
        }
    }
    Ok(())
}

/// One folder of an encoded header: its coders, refused if encrypted or
/// asking too large a dictionary. How many outputs it has.
fn seven_zip_folder(
    cursor: &mut HeaderCursor<'_>,
    limits: &ArchiveLimits,
) -> Result<u64, ArchiveError> {
    let coders = cursor.count()?;
    let (mut ins, mut outs) = (0_u64, 0_u64);
    for _ in 0..coders {
        let flags = cursor.byte()?;
        if flags & 0x80 != 0 {
            return Err(header_unreadable());
        }
        let id = cursor.take(usize::from(flags & 0x0F))?;
        if flags & 0x10 != 0 {
            ins += cursor.count()?;
            outs += cursor.count()?;
        } else {
            ins += 1;
            outs += 1;
        }
        if flags & 0x20 != 0 {
            let length = usize::try_from(cursor.count()?).map_err(|_| header_unreadable())?;
            let properties = cursor.take(length)?;
            if id == sevenz_rust2::EncoderMethod::ID_AES256_SHA256 {
                return Err(ArchiveError::Encrypted);
            }
            if let Some(needed) = dictionary_bytes(id, properties)
                && needed > limits.max_dictionary_bytes
            {
                return Err(ArchiveError::Dictionary {
                    needed,
                    limit: limits.max_dictionary_bytes,
                });
            }
        }
    }
    let pairs = outs.checked_sub(1).ok_or_else(header_unreadable)?;
    for _ in 0..pairs * 2 {
        cursor.number()?;
    }
    let packed = ins.checked_sub(pairs).ok_or_else(header_unreadable)?;
    if packed > 1 {
        for _ in 0..packed {
            cursor.number()?;
        }
    }
    Ok(outs)
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

/// What a tar guard refused, for the error the tar reader then returns.
type Refusal = std::rc::Rc<std::cell::RefCell<Option<ArchiveError>>>;

/// Between a tar's bytes and the tar reader, following its headers as they
/// pass: what the reader would hold whole in memory (a GNU long name or
/// link, a PAX record) is refused past [`MAX_TAR_EXTENSION_BYTES`] before the
/// reader reads it, as is an old GNU sparse file, whose extra headers would
/// put the count out of step; and the deadline is kept while the reader
/// skips data it does not hand on.
///
/// The guard must count each entry's data exactly as the reader does, or it
/// takes data for headers and headers for data. So a PAX `size` record is
/// followed as the reader follows it (tar 0.4: a local `x` header's size,
/// read with the reader's own record parser, replaces the size of the next
/// entry that is not itself an extension header; a global `g` header uses
/// it up; two `x` headers for one entry are an error). Missing that let a
/// PAX size of 0 hide a long name past the cap (security review,
/// 2026-11-12).
struct TarGuard<R> {
    inner: R,
    header: [u8; 512],
    filled: usize,
    data_left: u64,
    deadline: Option<Instant>,
    refusal: Refusal,
    /// A local PAX header's records, waiting for the entry they describe.
    pax: Option<Vec<u8>>,
    /// A local PAX header's records as they pass, and how many are to come.
    capture: Option<Vec<u8>>,
    capture_left: u64,
}

impl<R> TarGuard<R> {
    fn refuse(&self, error: ArchiveError) -> io::Error {
        let message = error.to_string();
        *self.refusal.borrow_mut() = Some(error);
        io::Error::other(message)
    }

    /// A whole header has passed: where its data ends, and whether it is
    /// one to refuse.
    fn header_passed(&mut self) -> Result<(), ArchiveError> {
        let header = &self.header;
        // The two zero blocks that end a tar.
        if header.iter().all(|byte| *byte == 0) {
            return Ok(());
        }
        let mut size = tar_size(&header[124..136]).ok_or_else(|| {
            ArchiveError::Corrupt("a tar header's size is not a number".to_owned())
        })?;
        let kind = header[156];
        match kind {
            b'L' | b'K' | b'x' | b'g' if size > MAX_TAR_EXTENSION_BYTES => {
                return Err(ArchiveError::Index("tar extension header"));
            }
            // The reader takes these as extensions only with a ustar or GNU
            // magic, and otherwise as ordinary entries; rather than follow
            // that, an extension header without one is refused.
            b'L' | b'K' | b'x' | b'g' if !recognised_tar_magic(header) => {
                return Err(ArchiveError::Corrupt(
                    "a tar extension header without a ustar or GNU magic".to_owned(),
                ));
            }
            b'x' => {
                if self.pax.is_some() || self.capture.is_some() {
                    return Err(ArchiveError::Corrupt(
                        "two PAX headers describe one tar entry".to_owned(),
                    ));
                }
                // At most MAX_TAR_EXTENSION_BYTES, refused above otherwise.
                self.capture = Some(Vec::with_capacity(usize::try_from(size).unwrap_or(0)));
                self.capture_left = size;
                if size == 0 {
                    self.pax = self.capture.take();
                }
            }
            // A global header is an entry to the reader, and uses up the
            // local records waiting, without taking their size.
            b'g' => self.pax = None,
            // Long names and links wait with the records for their entry.
            b'L' | b'K' => {}
            b'S' => {
                return Err(ArchiveError::NotAFile {
                    name: String::from_utf8_lossy(&header[..100])
                        .trim_end_matches('\0')
                        .to_owned(),
                    kind: "sparse file",
                });
            }
            // The entry the waiting records describe: their size, if they
            // give one, is the size the reader uses.
            _ => {
                if let Some(records) = self.pax.take()
                    && let Some(pax) = pax_size(&records)
                {
                    size = pax;
                }
            }
        }
        self.data_left = size
            .div_ceil(512)
            .checked_mul(512)
            .ok_or_else(|| ArchiveError::Corrupt("a tar entry's size is impossible".to_owned()))?;
        Ok(())
    }
}

/// Whether a tar header block carries the ustar or the GNU magic, as the
/// tar reader checks it before it takes the block for an extension header.
fn recognised_tar_magic(header: &[u8; 512]) -> bool {
    let (magic, version) = (&header[257..263], &header[263..265]);
    (magic == b"ustar\0" && version == b"00") || (magic == b"ustar " && version == b" \0")
}

/// A PAX `size` record, found exactly as the tar reader finds it (tar 0.4,
/// `pax_extensions_value`): the first record named `size`, with the reader's
/// own record parser; a malformed record before it, or a value that is not
/// a number, means none.
fn pax_size(records: &[u8]) -> Option<u64> {
    for record in tar::PaxExtensions::new(records) {
        let record = record.ok()?;
        if record.key() != Ok("size") {
            continue;
        }
        return record.value().ok()?.parse::<u64>().ok();
    }
    None
}

impl<R: Read> Read for TarGuard<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() > deadline)
        {
            return Err(self.refuse(ArchiveError::TimedOut));
        }
        let read = self.inner.read(buffer)?;
        let mut at = 0;
        while at < read {
            if self.data_left > 0 {
                let take = (read - at).min(usize::try_from(self.data_left).unwrap_or(usize::MAX));
                // A local PAX header's records, kept for the entry they
                // describe; its padding is not kept.
                if let Some(capture) = self.capture.as_mut() {
                    let keep = take.min(usize::try_from(self.capture_left).unwrap_or(usize::MAX));
                    capture.extend_from_slice(&buffer[at..at + keep]);
                    self.capture_left -= keep as u64;
                    if self.capture_left == 0 {
                        self.pax = self.capture.take();
                    }
                }
                self.data_left -= take as u64;
                at += take;
                continue;
            }
            let take = (read - at).min(512 - self.filled);
            self.header[self.filled..self.filled + take].copy_from_slice(&buffer[at..at + take]);
            self.filled += take;
            at += take;
            if self.filled == 512 {
                self.filled = 0;
                if let Err(error) = self.header_passed() {
                    return Err(self.refuse(error));
                }
            }
        }
        Ok(read)
    }
}

/// A tar header's size field: octal digits, or base-256 when the top bit of
/// its first byte is set (GNU, for sizes over 8 GiB).
fn tar_size(field: &[u8]) -> Option<u64> {
    if field.first()? & 0x80 != 0 {
        let mut value = u64::from(field[0] & 0x7F);
        for byte in &field[1..] {
            value = value.checked_mul(256)?.checked_add(u64::from(*byte))?;
        }
        return Some(value);
    }
    let text = std::str::from_utf8(field).ok()?;
    let digits = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, 8).ok()
}

fn unpack_tar(reader: impl Read, writer: &mut Writer<'_>) -> Result<(), ArchiveError> {
    let refusal = Refusal::default();
    let guard = TarGuard {
        inner: reader,
        header: [0; 512],
        filled: 0,
        data_left: 0,
        deadline: writer.limits.deadline,
        refusal: std::rc::Rc::clone(&refusal),
        pax: None,
        capture: None,
        capture_left: 0,
    };
    let outcome = unpack_tar_entries(guard, writer);
    // What the guard refused is the reason, whatever the reader made of it.
    let refused = refusal.borrow_mut().take();
    match refused {
        Some(error) => Err(error),
        None => outcome,
    }
}

fn unpack_tar_entries(reader: impl Read, writer: &mut Writer<'_>) -> Result<(), ArchiveError> {
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
        // A PAX sparse file is an ordinary entry whose data is the sparse
        // map and the extents, not the file.
        let sparse = entry
            .pax_extensions()
            .map_err(|error| corrupt(&error))?
            .is_some_and(|mut extensions| {
                extensions.any(|extension| {
                    extension.is_ok_and(|extension| {
                        extension
                            .key()
                            .is_ok_and(|key| key.starts_with("GNU.sparse."))
                    })
                })
            });
        if sparse {
            return Err(ArchiveError::NotAFile {
                name,
                kind: "sparse file",
            });
        }
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
