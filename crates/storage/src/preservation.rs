// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Preservation hashes (docs/plans/dat-matching.md): CRC32, MD5 and SHA-1, the
//! hashes preservation databases such as Redump list, of a whole file and of
//! byte ranges within it, in one sequential pass.
//!
//! SHA-256 stays the identity of stored bytes; these are for matching. A CD
//! image's tracks are ranges of its BIN, and Redump lists each track's
//! hashes, so the ranges are hashed as the file goes past rather than read
//! again.

use std::path::Path;

use md5::Digest as _;
use tokio::io::AsyncReadExt as _;

/// CRC32 (as zip and Redump compute it), MD5 and SHA-1 of some bytes, in
/// lowercase hex, and how many bytes they cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservationHashes {
    /// CRC-32/ISO-HDLC, eight hex digits.
    pub crc32: String,
    /// MD5, 32 hex digits.
    pub md5: String,
    /// SHA-1, 40 hex digits.
    pub sha1: String,
    /// Bytes covered.
    pub length: u64,
}

/// The three hashes, fed as the bytes go past.
struct Hasher {
    crc: crc::Digest<'static, u32>,
    md5: md5::Md5,
    sha1: sha1::Sha1,
    length: u64,
}

/// CRC-32 as zip computes it, which is what DATs list.
static CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

impl Hasher {
    fn new() -> Self {
        Self {
            crc: CRC32.digest(),
            md5: md5::Md5::new(),
            sha1: sha1::Sha1::new(),
            length: 0,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.crc.update(bytes);
        self.md5.update(bytes);
        self.sha1.update(bytes);
        self.length += bytes.len() as u64;
    }

    fn finish(self) -> PreservationHashes {
        PreservationHashes {
            crc32: format!("{:08x}", self.crc.finalize()),
            md5: hex::encode(self.md5.finalize()),
            sha1: hex::encode(self.sha1.finalize()),
            length: self.length,
        }
    }
}

/// The hashes of `bytes`, for tests and small inputs.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> PreservationHashes {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// What hashing a file found: its whole hashes, and each range's in the
/// order asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHashes {
    /// The whole file.
    pub whole: PreservationHashes,
    /// Each range, `[start, end)`, clipped to the file.
    pub ranges: Vec<PreservationHashes>,
}

/// Hash the file at `path` and each of `ranges` (`[start, end)` byte
/// offsets) in one pass, reading 1 MiB at a time. A range reaching past the
/// end of the file covers what there is, which its length says.
///
/// # Errors
///
/// An I/O error opening or reading the file.
pub async fn hash_file(path: &Path, ranges: &[(u64, u64)]) -> std::io::Result<FileHashes> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut whole = Hasher::new();
    let mut hashers: Vec<Hasher> = ranges.iter().map(|_| Hasher::new()).collect();
    let mut buffer = vec![0_u8; 1 << 20];
    let mut offset = 0_u64;
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        let chunk_end = offset + read as u64;
        whole.update(chunk);
        for (hasher, (start, end)) in hashers.iter_mut().zip(ranges) {
            let from = (*start).max(offset);
            let to = (*end).min(chunk_end);
            if from < to {
                let a = usize::try_from(from - offset).unwrap_or(0);
                let b = usize::try_from(to - offset).unwrap_or(0);
                hasher.update(&chunk[a..b]);
            }
        }
        offset = chunk_end;
    }
    Ok(FileHashes {
        whole: whole.finish(),
        ranges: hashers.into_iter().map(Hasher::finish).collect(),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn known_inputs_hash_to_their_published_values() {
        let empty = hash_bytes(b"");
        assert_eq!(empty.crc32, "00000000");
        assert_eq!(empty.md5, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(empty.sha1, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        let fox = hash_bytes(b"The quick brown fox jumps over the lazy dog");
        assert_eq!(fox.crc32, "414fa339");
        assert_eq!(fox.md5, "9e107d9d372bb6826bd81d3542a419d6");
        assert_eq!(fox.sha1, "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12");
        assert_eq!(fox.length, 43);
    }

    #[tokio::test]
    async fn ranges_hash_as_their_bytes_alone_would_across_read_boundaries() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("disc.bin");
        // Larger than one read, so ranges cross the boundary between reads.
        let bytes: Vec<u8> = (0_u32..(3 << 20) + 17)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        std::fs::write(&path, &bytes).unwrap();
        let ranges = [
            (0, 2352 * 300),
            (2352 * 300, (1 << 20) + 5),
            ((1 << 20) + 5, bytes.len() as u64),
            (10, 10),
            (bytes.len() as u64 - 4, bytes.len() as u64 + 100),
        ];
        let found = hash_file(&path, &ranges).await.unwrap();
        assert_eq!(found.whole, hash_bytes(&bytes));
        for ((start, end), hashes) in ranges.iter().zip(&found.ranges) {
            let end = (*end).min(bytes.len() as u64);
            let slice = &bytes[usize::try_from(*start).unwrap()..usize::try_from(end).unwrap()];
            assert_eq!(*hashes, hash_bytes(slice), "{start}..{end}");
        }
    }
}
