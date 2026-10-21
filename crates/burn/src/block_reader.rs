// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reading a DVD or Blu-ray data disc into an image, sector by sector
//! (ADR 0013, phase R2).
//!
//! No tool: the worker already holds the drive's block device, and the
//! kernel returns the disc's 2048-byte sectors in order. The device's size
//! is where its readable area ends. A chunk that fails is read again a
//! sector at a time, each sector a few times; one that still fails stops
//! the read, because an image with a hole in it is not the disc.
//!
//! Every sector is also checked for CSS scrambling, the flag in a DVD-Video
//! sector's MPEG header. A scrambled sector stops the read: until
//! decryption is built (R3), an encrypted disc is refused, never kept as
//! scrambled bytes that look like content.
//!
//! Many drives never return a scrambled sector at all: without the
//! authentication a player performs, they answer a read of one with an
//! error. Found on the bench with a commercial DVD: its menus read clear,
//! and from the second sector of its title video on, every read failed.
//! (The sector the error was blamed on was not one of them: the kernel's
//! readahead had reached into the refused video.) So when a sector will
//! not read, the disc's own ISO 9660 directory, which reads fine, is
//! asked for its title video: a DVD-Video disc whose largest `.VOB` file
//! refuses to be read, or reads scrambled, is CSS-encrypted, and is
//! refused as that rather than reported as damaged.

use std::io::SeekFrom;
use std::path::Path;

use tokio::fs;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

use crate::engine::{BurnEvent, EngineError, EventSink};
use crate::fake::CancelToken;

/// Bytes in a DVD or Blu-ray sector.
pub const SECTOR_BYTES: usize = 2048;

/// Sectors read at a time while nothing goes wrong: 1 MiB.
const CHUNK_SECTORS: usize = 512;

/// Times one sector is tried before the read gives up on it.
const SECTOR_TRIES: u32 = 4;

/// How an image read went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRead {
    /// Bytes written to the image.
    pub bytes: u64,
    /// Sectors that read only after retrying.
    pub retried_sectors: u64,
}

/// Whether a sector is CSS-scrambled DVD-Video; in the domain crate so the
/// image inspector can ask the same question of an imported VIDEO_TS tree.
pub use tangible_domain::dvd::css_scrambled;

/// Read `device` into `output`, which must not exist yet.
///
/// # Errors
///
/// [`EngineError::NoMedium`] when the device has nothing readable,
/// [`EngineError::Encrypted`] at the first scrambled sector,
/// [`EngineError::Unsupported`] naming the first sector that would not read,
/// [`EngineError::Cancelled`], or [`EngineError::Io`]. On any of these the
/// partial image is removed.
pub async fn read_image(
    device: &Path,
    output: &Path,
    expected_bytes: Option<u64>,
    cancel: &CancelToken,
    sink: &dyn EventSink,
) -> Result<ImageRead, EngineError> {
    // Refused before anything is opened, so the clean-up below only ever
    // removes an image this read began.
    if fs::try_exists(output).await.unwrap_or(true) {
        return Err(EngineError::Io {
            operation: "creating the image",
            source: std::io::Error::from(std::io::ErrorKind::AlreadyExists),
        });
    }
    let result = read_into(device, output, expected_bytes, cancel, sink).await;
    if result.is_err() {
        let _ = fs::remove_file(output).await;
    }
    result
}

async fn read_into(
    device: &Path,
    output: &Path,
    expected_bytes: Option<u64>,
    cancel: &CancelToken,
    sink: &dyn EventSink,
) -> Result<ImageRead, EngineError> {
    let io = |operation: &'static str| move |source| EngineError::Io { operation, source };
    let mut disc = fs::File::open(device)
        .await
        .map_err(io("opening the drive"))?;
    // A block device's end is where its readable area ends.
    let size = disc
        .seek(SeekFrom::End(0))
        .await
        .map_err(io("finding the end of the disc"))?;
    if size == 0 {
        return Err(EngineError::NoMedium {
            alias: device.display().to_string(),
        });
    }
    // The kernel learns a disc's size when the disc is put in, not when a
    // drive finishes burning one, and reads stop at the size it knows. Found
    // on the bench: straight after a burn it still said 2048 bytes, and an
    // image of that would have been a fragment called a disc.
    if let Some(expected) = expected_bytes
        && size < expected
    {
        return Err(EngineError::UnsuitableMedium {
            reason: format!(
                "the system still reports {size} bytes for a disc the drive says holds \
                 {expected}; take the disc out and put it back, then ask again"
            ),
        });
    }
    disc.seek(SeekFrom::Start(0))
        .await
        .map_err(io("returning to the start of the disc"))?;
    let mut image = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .await
        .map_err(io("creating the image"))?;

    sink.emit(BurnEvent::new(
        "ripping",
        "RIP_STARTED",
        format!("reading {size} bytes from the disc"),
    ));
    let mut buffer = vec![0_u8; CHUNK_SECTORS * SECTOR_BYTES];
    let mut offset = 0_u64;
    let mut retried = 0_u64;
    let mut last_percent = None;
    while offset < size {
        if cancel.is_cancelled() {
            sink.emit(BurnEvent::new(
                "ripping",
                "RIP_CANCELLED",
                "the rip was withdrawn",
            ));
            return Err(EngineError::Cancelled);
        }
        let want =
            usize::try_from((size - offset).min(buffer.len() as u64)).unwrap_or(buffer.len());
        let chunk = &mut buffer[..want];
        retried += read_chunk(&mut disc, offset, chunk, sink).await?;
        if let Some(index) = chunk.chunks(SECTOR_BYTES).position(css_scrambled) {
            sink.emit(BurnEvent::new(
                "ripping",
                "RIP_ENCRYPTED",
                format!(
                    "sector {} is CSS-scrambled; encrypted discs are not ripped yet",
                    offset / SECTOR_BYTES as u64 + index as u64
                ),
            ));
            return Err(EngineError::Encrypted { system: "CSS" });
        }
        image
            .write_all(chunk)
            .await
            .map_err(io("writing the image"))?;
        offset += want as u64;

        // A whole percent, rounded down, so 100 is said only at the end.
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let fraction = (offset as f64 / size as f64) as f32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let percent = (fraction * 100.0) as u32;
        if last_percent != Some(percent) {
            last_percent = Some(percent);
            sink.emit(BurnEvent::progress(
                "ripping",
                "RIP_PROGRESS",
                fraction,
                format!("{percent}% read"),
            ));
        }
    }
    image.flush().await.map_err(io("writing the image"))?;
    image.sync_all().await.map_err(io("writing the image"))?;
    sink.emit(BurnEvent::new(
        "ripping",
        "RIP_READ",
        format!("read {offset} bytes"),
    ));
    Ok(ImageRead {
        bytes: offset,
        retried_sectors: retried,
    })
}

/// Read one chunk, falling back to a sector at a time when it fails whole.
/// Answers how many sectors needed more than one try.
async fn read_chunk(
    disc: &mut fs::File,
    offset: u64,
    chunk: &mut [u8],
    sink: &dyn EventSink,
) -> Result<u64, EngineError> {
    if read_at(disc, offset, chunk).await.is_ok() {
        return Ok(0);
    }
    match read_sectors_slowly(disc, offset, chunk).await {
        Err(error @ EngineError::Encrypted { .. }) => {
            sink.emit(BurnEvent::new(
                "ripping",
                "RIP_ENCRYPTED",
                "the drive will not read the disc's video files without authentication: it is \
                 CSS-encrypted",
            ));
            Err(error)
        }
        other => other,
    }
}

/// Fill `buffer` from `offset`, or fail.
async fn read_at(disc: &mut fs::File, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
    disc.seek(SeekFrom::Start(offset)).await?;
    disc.read_exact(buffer).await.map(|_| ())
}

/// Read a chunk that failed whole, a sector at a time, trying each a few
/// times. Answers how many sectors needed more than one try.
async fn read_sectors_slowly(
    disc: &mut fs::File,
    offset: u64,
    chunk: &mut [u8],
) -> Result<u64, EngineError> {
    let mut retried = 0;
    for (index, sector) in chunk.chunks_mut(SECTOR_BYTES).enumerate() {
        let at = offset + (index * SECTOR_BYTES) as u64;
        let mut tries = 0;
        loop {
            tries += 1;
            if read_at(disc, at, sector).await.is_ok() {
                break;
            }
            if tries >= SECTOR_TRIES {
                if title_video_refused(disc).await {
                    return Err(EngineError::Encrypted { system: "CSS" });
                }
                return Err(EngineError::Unsupported {
                    what: format!(
                        "sector {} could not be read after {SECTOR_TRIES} tries; the disc may be \
                         damaged, or encrypted in a way the drive will not read without \
                         authentication",
                        at / SECTOR_BYTES as u64
                    ),
                });
            }
        }
        if tries > 1 {
            retried += 1;
        }
    }
    Ok(retried)
}

/// Most sectors of a directory read when looking for video files.
const MAX_DIRECTORY_SECTORS: u64 = 64;

/// One entry of an ISO 9660 directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectoryEntry {
    name: String,
    extent: u64,
    bytes: u64,
    directory: bool,
}

/// The entries of an ISO 9660 directory's data, skipping `.` and `..`.
fn directory_entries(data: &[u8]) -> Vec<DirectoryEntry> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let length = usize::from(data[at]);
        if length == 0 {
            // Records never cross a sector; zeros pad to the next one.
            at = (at / SECTOR_BYTES + 1) * SECTOR_BYTES;
            continue;
        }
        let Some(record) = data.get(at..at + length) else {
            break;
        };
        at += length;
        if record.len() < 34 {
            continue;
        }
        let name_length = usize::from(record[32]);
        let Some(name) = record.get(33..33 + name_length) else {
            continue;
        };
        if name == [0] || name == [1] {
            continue;
        }
        let le =
            |bytes: &[u8]| u64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
        entries.push(DirectoryEntry {
            name: String::from_utf8_lossy(name).into_owned(),
            extent: le(&record[2..6]),
            bytes: le(&record[10..14]),
            directory: record[25] & 0x02 != 0,
        });
    }
    entries
}

/// Read a directory's data, bounded.
async fn read_directory(disc: &mut fs::File, extent: u64, bytes: u64) -> Option<Vec<u8>> {
    let sectors = bytes
        .div_ceil(SECTOR_BYTES as u64)
        .min(MAX_DIRECTORY_SECTORS);
    let mut data = vec![0_u8; usize::try_from(sectors).ok()? * SECTOR_BYTES];
    read_at(disc, extent * SECTOR_BYTES as u64, &mut data)
        .await
        .ok()?;
    Some(data)
}

/// The sectors of every `.VOB` file under `VIDEO_TS`, from the disc's ISO
/// 9660 directory. Empty for anything that is not shaped like DVD-Video.
async fn vob_extents(disc: &mut fs::File) -> Vec<std::ops::Range<u64>> {
    let mut pvd = vec![0_u8; SECTOR_BYTES];
    if read_at(disc, 16 * SECTOR_BYTES as u64, &mut pvd)
        .await
        .is_err()
        || pvd[0] != 1
        || &pvd[1..6] != b"CD001"
    {
        return Vec::new();
    }
    let root = &pvd[156..190];
    let le = |bytes: &[u8]| u64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
    let Some(root_data) = read_directory(disc, le(&root[2..6]), le(&root[10..14])).await else {
        return Vec::new();
    };
    let Some(video_ts) = directory_entries(&root_data)
        .into_iter()
        .find(|entry| entry.directory && entry.name.eq_ignore_ascii_case("VIDEO_TS"))
    else {
        return Vec::new();
    };
    let Some(video_data) = read_directory(disc, video_ts.extent, video_ts.bytes).await else {
        return Vec::new();
    };
    directory_entries(&video_data)
        .into_iter()
        .filter(|entry| {
            !entry.directory
                && entry
                    .name
                    .split(';')
                    .next()
                    .is_some_and(|name| name.to_ascii_uppercase().ends_with(".VOB"))
        })
        .map(|entry| entry.extent..entry.extent + entry.bytes.div_ceil(SECTOR_BYTES as u64))
        .collect()
}

/// Sectors probed into a title video, past its first, which is often
/// clear even on an encrypted disc.
const PROBE_OFFSETS: [u64; 4] = [1, 2, 16, 64];

/// Whether this is a DVD-Video disc whose title video the drive will not
/// hand over, or hands over scrambled: what a CSS disc does without
/// authentication.
async fn title_video_refused(disc: &mut fs::File) -> bool {
    let Some(largest) = vob_extents(disc)
        .await
        .into_iter()
        .max_by_key(|extent| extent.end - extent.start)
    else {
        return false;
    };
    let mut sector = vec![0_u8; SECTOR_BYTES];
    for offset in PROBE_OFFSETS {
        let number = largest.start + offset;
        if number >= largest.end {
            break;
        }
        match read_at(disc, number * SECTOR_BYTES as u64, &mut sector).await {
            Err(_) => return true,
            Ok(()) if css_scrambled(&sector) => return true,
            Ok(()) => {}
        }
    }
    false
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::engine::CollectingSink;

    /// A DVD-Video sector: a pack header with `stuffing` bytes, then a PES
    /// packet for `stream` with the scrambling bits set to `scrambling`.
    fn vob_sector(stream: u8, scrambling: u8, stuffing: u8) -> Vec<u8> {
        let mut sector = vec![0_u8; SECTOR_BYTES];
        sector[..4].copy_from_slice(&[0x00, 0x00, 0x01, 0xBA]);
        sector[4] = 0x44;
        sector[13] = 0xF8 | stuffing;
        let pes = 14 + usize::from(stuffing);
        sector[pes..pes + 4].copy_from_slice(&[0x00, 0x00, 0x01, stream]);
        sector[pes + 6] = 0x80 | (scrambling << 4);
        sector
    }

    /// One directory record.
    fn record(name: &[u8], extent: u32, bytes: u32, directory: bool) -> Vec<u8> {
        let mut record = vec![0_u8; 33 + name.len() + usize::from(name.len().is_multiple_of(2))];
        record[0] = u8::try_from(record.len()).unwrap();
        record[2..6].copy_from_slice(&extent.to_le_bytes());
        record[6..10].copy_from_slice(&extent.to_be_bytes());
        record[10..14].copy_from_slice(&bytes.to_le_bytes());
        record[14..18].copy_from_slice(&bytes.to_be_bytes());
        record[25] = if directory { 2 } else { 0 };
        record[32] = u8::try_from(name.len()).unwrap();
        record[33..33 + name.len()].copy_from_slice(name);
        record
    }

    /// A DVD-Video-shaped ISO 9660 image: the root at sector 20, VIDEO_TS at
    /// 21, VIDEO_TS.IFO at 30 and two VOBs at 40 (3 sectors) and 100 (2).
    fn dvd_video_image() -> Vec<u8> {
        let mut image = vec![0_u8; SECTOR_BYTES * 120];
        let sector = |n: usize| n * SECTOR_BYTES;
        image[sector(16)] = 1;
        image[sector(16) + 1..sector(16) + 6].copy_from_slice(b"CD001");
        let root = record(&[0], 20, 2048, true);
        image[sector(16) + 156..sector(16) + 156 + root.len()].copy_from_slice(&root);
        let mut at = sector(20);
        for entry in [
            record(&[0], 20, 2048, true),
            record(&[1], 20, 2048, true),
            record(b"AUDIO_TS", 22, 2048, true),
            record(b"VIDEO_TS", 21, 2048, true),
        ] {
            image[at..at + entry.len()].copy_from_slice(&entry);
            at += entry.len();
        }
        at = sector(21);
        for entry in [
            record(&[0], 21, 2048, true),
            record(&[1], 20, 2048, true),
            record(b"VIDEO_TS.IFO;1", 30, 4096, false),
            record(b"VTS_01_1.VOB;1", 40, 3 * 2048, false),
            record(b"vts_01_2.vob;1", 100, 2048 + 1, false),
        ] {
            image[at..at + entry.len()].copy_from_slice(&entry);
            at += entry.len();
        }
        image
    }

    #[tokio::test]
    async fn the_video_files_of_a_dvd_are_found_from_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dvd");
        std::fs::write(&path, dvd_video_image()).unwrap();
        let mut disc = fs::File::open(&path).await.unwrap();
        assert_eq!(vob_extents(&mut disc).await, vec![40..43, 100..102]);
    }

    #[tokio::test]
    async fn a_title_video_that_reads_scrambled_is_refused_video() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dvd");
        let mut image = dvd_video_image();
        std::fs::write(&path, &image).unwrap();
        let mut clear = fs::File::open(&path).await.unwrap();
        assert!(
            !title_video_refused(&mut clear).await,
            "clear video is not refused"
        );
        // The largest VOB, at 40, scrambled from its second sector.
        image[41 * SECTOR_BYTES..42 * SECTOR_BYTES].copy_from_slice(&vob_sector(0xE0, 1, 0));
        std::fs::write(&path, &image).unwrap();
        let mut scrambled = fs::File::open(&path).await.unwrap();
        assert!(title_video_refused(&mut scrambled).await);
        // Cut short inside the VOB: its sectors no longer read.
        std::fs::write(&path, &image[..41 * SECTOR_BYTES]).unwrap();
        let mut refused = fs::File::open(&path).await.unwrap();
        assert!(
            title_video_refused(&mut refused).await,
            "an unreadable title is refused video"
        );
    }

    #[tokio::test]
    async fn a_disc_that_is_not_dvd_video_has_no_video_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut image = dvd_video_image();
        // Rename VIDEO_TS: a data disc with a directory of another name.
        let at = image
            .windows(8)
            .position(|window| window == b"VIDEO_TS")
            .unwrap();
        image[at..at + 8].copy_from_slice(b"VIDEO_XX");
        std::fs::write(&path, &image).unwrap();
        let mut disc = fs::File::open(&path).await.unwrap();
        assert!(vob_extents(&mut disc).await.is_empty());
        std::fs::write(&path, vec![0_u8; SECTOR_BYTES * 40]).unwrap();
        let mut blank = fs::File::open(&path).await.unwrap();
        assert!(vob_extents(&mut blank).await.is_empty(), "no volume at all");
    }

    #[test]
    fn a_scrambled_vob_sector_is_recognised() {
        for stream in [0xE0, 0xBD, 0xC0] {
            assert!(css_scrambled(&vob_sector(stream, 1, 0)), "{stream:#x}");
            assert!(
                css_scrambled(&vob_sector(stream, 3, 3)),
                "{stream:#x} with stuffing"
            );
        }
    }

    #[test]
    fn plain_sectors_are_not_scrambled() {
        assert!(!css_scrambled(&vob_sector(0xE0, 0, 0)), "clear video");
        assert!(
            !css_scrambled(&vob_sector(0xBF, 1, 0)),
            "navigation is never scrambled"
        );
        assert!(!css_scrambled(&vec![0_u8; SECTOR_BYTES]));
        let mut iso = vec![0_u8; SECTOR_BYTES];
        iso[1..6].copy_from_slice(b"CD001");
        assert!(!css_scrambled(&iso));
        let mut mpeg1 = vob_sector(0xE0, 1, 0);
        mpeg1[4] = 0x21;
        assert!(!css_scrambled(&mpeg1), "an MPEG-1 pack is not a DVD sector");
        assert!(!css_scrambled(&[0, 0, 1, 0xBA]));
    }

    async fn read(device: &Path, dir: &Path) -> Result<ImageRead, EngineError> {
        read_image(
            device,
            &dir.join("disc.iso"),
            None,
            &CancelToken::new(),
            &CollectingSink::new(),
        )
        .await
    }

    #[tokio::test]
    async fn a_size_the_system_has_not_caught_up_with_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        std::fs::write(&device, vec![0_u8; SECTOR_BYTES]).unwrap();
        let result = read_image(
            &device,
            &dir.path().join("disc.iso"),
            Some(2384 * SECTOR_BYTES as u64),
            &CancelToken::new(),
            &CollectingSink::new(),
        )
        .await;
        let Err(EngineError::UnsuitableMedium { reason }) = result else {
            panic!("{result:?}");
        };
        assert!(
            reason.contains("take the disc out and put it back"),
            "{reason}"
        );
        assert!(!dir.path().join("disc.iso").exists());
        // The system agreeing is fine.
        assert!(
            read_image(
                &device,
                &dir.path().join("disc.iso"),
                Some(SECTOR_BYTES as u64),
                &CancelToken::new(),
                &CollectingSink::new(),
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn a_plain_image_is_read_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        let bytes: Vec<u8> = (0..SECTOR_BYTES * 1300)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        std::fs::write(&device, &bytes).unwrap();
        let sink = CollectingSink::new();
        let read = read_image(
            &device,
            &dir.path().join("disc.iso"),
            None,
            &CancelToken::new(),
            &sink,
        )
        .await
        .unwrap();
        assert_eq!(read.bytes, bytes.len() as u64);
        assert_eq!(read.retried_sectors, 0);
        assert_eq!(std::fs::read(dir.path().join("disc.iso")).unwrap(), bytes);
        let progress: Vec<String> = sink
            .events()
            .iter()
            .filter(|event| event.code == "RIP_PROGRESS")
            .map(|event| event.message.clone())
            .collect();
        assert_eq!(progress.last().map(String::as_str), Some("100% read"));
        assert!(progress.len() <= 101);
    }

    #[tokio::test]
    async fn an_encrypted_disc_stops_the_read_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        let mut bytes = vec![0_u8; SECTOR_BYTES * 600];
        bytes[SECTOR_BYTES * 550..SECTOR_BYTES * 551].copy_from_slice(&vob_sector(0xE0, 1, 0));
        std::fs::write(&device, &bytes).unwrap();
        let result = read(&device, dir.path()).await;
        assert!(
            matches!(result, Err(EngineError::Encrypted { system: "CSS" })),
            "{result:?}"
        );
        assert!(!dir.path().join("disc.iso").exists());
    }

    #[tokio::test]
    async fn nothing_readable_is_no_medium() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        std::fs::write(&device, b"").unwrap();
        assert!(matches!(
            read(&device, dir.path()).await,
            Err(EngineError::NoMedium { .. })
        ));
    }

    #[tokio::test]
    async fn a_withdrawn_read_stops_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        std::fs::write(&device, vec![0_u8; SECTOR_BYTES * 2000]).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        let result = read_image(
            &device,
            &dir.path().join("disc.iso"),
            None,
            &cancel,
            &CollectingSink::new(),
        )
        .await;
        assert!(matches!(result, Err(EngineError::Cancelled)));
        assert!(!dir.path().join("disc.iso").exists());
    }

    #[tokio::test]
    async fn an_existing_image_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        std::fs::write(&device, vec![1_u8; SECTOR_BYTES]).unwrap();
        std::fs::write(dir.path().join("disc.iso"), b"kept").unwrap();
        let result = read_image(
            &device,
            &dir.path().join("disc.iso"),
            None,
            &CancelToken::new(),
            &CollectingSink::new(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(dir.path().join("disc.iso")).unwrap(), b"kept");
    }
}
