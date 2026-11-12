// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Unpacking archives, against fixtures made by the real tools
//! (`fixtures/images/archives/make_fixtures.py`).

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use tangible_image::archive::{ArchiveError, ArchiveKind, ArchiveLimits, detect, unpack};
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}/../../fixtures/images/archives/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
}

const SECTOR: usize = 2352;

fn bin() -> Vec<u8> {
    (0..SECTOR * 4)
        .map(|i| u8::try_from((i * 7 + 3) % 256).unwrap())
        .collect()
}

fn limits() -> ArchiveLimits {
    ArchiveLimits::for_import(1 << 30)
}

fn unpacked(name: &str) -> Result<(TempDir, tangible_image::archive::Unpacked), ArchiveError> {
    let dir = TempDir::new().unwrap();
    let kind = detect(&fixture(name)).unwrap().expect("an archive");
    let out = unpack(&fixture(name), kind, dir.path(), &limits())?;
    Ok((dir, out))
}

#[test]
fn every_format_is_recognised_by_its_bytes() {
    for (name, kind) in [
        ("game.zip", ArchiveKind::Zip),
        ("game.7z", ArchiveKind::SevenZip),
        ("game.tar", ArchiveKind::Tar),
        ("game.tar.gz", ArchiveKind::TarGz),
        ("game.tar.xz", ArchiveKind::TarXz),
    ] {
        assert_eq!(detect(&fixture(name)).unwrap(), Some(kind), "{name}");
    }
    // Not by name: an archive's name on something else is just a file.
    let dir = TempDir::new().unwrap();
    let pretender = dir.path().join("game.zip");
    std::fs::write(&pretender, vec![0_u8; 4096]).unwrap();
    assert_eq!(detect(&pretender).unwrap(), None);
    // A compressed file that is not a tar is not an archive here.
    let gz = dir.path().join("disc.iso.gz");
    let mut encoder = flate2_like_gzip(&[0x5a_u8; 8192]);
    std::fs::write(&gz, &mut encoder).unwrap();
    assert_eq!(detect(&gz).unwrap(), None);
}

/// A gzip stream of `bytes`, made with the system's gzip.
fn flate2_like_gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("gzip")
        .arg("-c")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("gzip");
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    child.wait_with_output().unwrap().stdout
}

#[test]
fn every_format_unpacks_the_same_files_byte_for_byte() {
    for name in [
        "game.zip",
        "game.7z",
        "game.tar",
        "game.tar.gz",
        "game.tar.xz",
    ] {
        let (dir, out) = unpacked(name).unwrap_or_else(|error| panic!("{name}: {error}"));
        let mut files: Vec<(String, u64)> = out
            .files
            .iter()
            .map(|(path, length)| (path.as_str().to_owned(), *length))
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec![
                ("Game/Game.bin".to_owned(), (SECTOR * 4) as u64),
                ("Game/Game.cue".to_owned(), 70),
            ],
            "{name}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("Game/Game.bin")).unwrap(),
            bin(),
            "{name}"
        );
        assert!(
            std::fs::read_to_string(dir.path().join("Game/Game.cue"))
                .unwrap()
                .starts_with("FILE \"Game.bin\" BINARY"),
            "{name}"
        );
        assert_eq!(out.expanded_bytes, (SECTOR * 4 + 70) as u64);
    }
}

#[test]
fn a_name_that_escapes_is_refused() {
    let error = unpacked("escape.zip").unwrap_err();
    assert!(matches!(error, ArchiveError::Path { .. }), "{error}");
    assert_eq!(error.code(), "ARCHIVE_UNSAFE_ENTRY");
}

#[test]
fn links_are_refused() {
    for name in ["symlink.zip", "hardlink.tar"] {
        let error = unpacked(name).unwrap_err();
        assert!(
            matches!(error, ArchiveError::NotAFile { .. }),
            "{name}: {error}"
        );
        assert_eq!(error.code(), "ARCHIVE_UNSAFE_ENTRY");
    }
}

#[test]
fn a_name_twice_is_refused_and_nothing_is_overwritten() {
    let error = unpacked("twice.tar").unwrap_err();
    assert!(matches!(error, ArchiveError::Duplicate(_)), "{error}");
    // A ZIP's directory is read one entry per name, as every ZIP reader
    // does: one file comes out, and nothing is written twice.
    let (_dir, out) = unpacked("twice.zip").unwrap();
    assert_eq!(out.files.len(), 1);
}

#[test]
fn encrypted_archives_are_refused() {
    for name in ["encrypted.zip", "encrypted.7z"] {
        let error = unpacked(name).unwrap_err();
        assert!(matches!(error, ArchiveError::Encrypted), "{name}: {error}");
        assert_eq!(error.code(), "ARCHIVE_ENCRYPTED");
    }
}

#[test]
fn macos_leftovers_are_left_out_and_an_inner_archive_is_a_file() {
    let (dir, out) = unpacked("macos.zip").unwrap();
    let names: Vec<&str> = out.files.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(names, ["Game.bin", "inner.zip"]);
    assert!(!dir.path().join("__MACOSX").exists());
    assert!(
        out.notes.iter().any(|note| note.starts_with("macOS")),
        "{:?}",
        out.notes
    );
    assert!(
        out.notes.iter().any(|note| note.contains("inner.zip")),
        "{:?}",
        out.notes
    );
}

#[test]
fn limits_are_kept() {
    let dir = TempDir::new().unwrap();
    let archive = fixture("game.zip");

    let few = ArchiveLimits {
        max_entries: 2,
        ..limits()
    };
    assert!(matches!(
        unpack(&archive, ArchiveKind::Zip, dir.path(), &few),
        Err(ArchiveError::TooManyEntries(2))
    ));

    let dir = TempDir::new().unwrap();
    let small = ArchiveLimits {
        max_expanded_bytes: 1000,
        ratio_floor_bytes: 0,
        ..limits()
    };
    let error = unpack(&archive, ArchiveKind::Zip, dir.path(), &small).unwrap_err();
    assert!(matches!(error, ArchiveError::TooLarge(1000)), "{error}");
    assert_eq!(error.code(), "ARCHIVE_TOO_LARGE");

    // The 735-byte zip unpacks to about 9.5 KB: over a ratio of 2.
    let dir = TempDir::new().unwrap();
    let ratio = ArchiveLimits {
        max_ratio: 2,
        ratio_floor_bytes: 0,
        ..limits()
    };
    assert!(matches!(
        unpack(&archive, ArchiveKind::Zip, dir.path(), &ratio),
        Err(ArchiveError::Ratio(2))
    ));

    let dir = TempDir::new().unwrap();
    let dictionary = ArchiveLimits {
        max_dictionary_bytes: 1,
        ..limits()
    };
    assert!(matches!(
        unpack(
            &fixture("game.7z"),
            ArchiveKind::SevenZip,
            dir.path(),
            &dictionary
        ),
        Err(ArchiveError::Dictionary { limit: 1, .. })
    ));

    let dir = TempDir::new().unwrap();
    let late = ArchiveLimits {
        deadline: Some(std::time::Instant::now()),
        ..limits()
    };
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(matches!(
        unpack(&archive, ArchiveKind::Zip, dir.path(), &late),
        Err(ArchiveError::TimedOut)
    ));
}

#[test]
fn only_an_empty_destination_is_unpacked_into() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("already"), b"here").unwrap();
    assert!(matches!(
        unpack(
            &fixture("game.zip"),
            ArchiveKind::Zip,
            dir.path(),
            &limits()
        ),
        Err(ArchiveError::Io { .. })
    ));
}

#[test]
fn a_damaged_archive_says_so() {
    let dir = TempDir::new().unwrap();
    let mut bytes = std::fs::read(fixture("game.7z")).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    let damaged = dir.path().join("damaged.7z");
    std::fs::write(&damaged, bytes).unwrap();
    let out = TempDir::new().unwrap();
    let error = unpack(&damaged, ArchiveKind::SevenZip, out.path(), &limits()).unwrap_err();
    assert_eq!(error.code(), "ARCHIVE_CORRUPT", "{error}");
}

#[test]
fn names_under_dot_slash_unpack_as_they_are() {
    let (dir, out) = unpacked("dotslash.tar").unwrap();
    let names: Vec<&str> = out.files.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(names, ["Game.bin"]);
    assert_eq!(std::fs::read(dir.path().join("Game.bin")).unwrap(), bin());
}

#[test]
fn a_pax_sparse_file_is_refused() {
    let error = unpacked("sparse.tar").unwrap_err();
    assert!(
        matches!(
            error,
            ArchiveError::NotAFile {
                kind: "sparse file",
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn a_file_where_a_folder_goes_is_final_not_retried() {
    let error = unpacked("clash.zip").unwrap_err();
    assert!(matches!(error, ArchiveError::Duplicate(_)), "{error}");
    assert_eq!(
        error.code(),
        "ARCHIVE_UNSAFE_ENTRY",
        "not ARCHIVE_IO, which is retried"
    );
}

/// A tar header block, its checksum filled in.
fn tar_header(name: &str, kind: u8, size_field: &[u8]) -> Vec<u8> {
    let mut block = vec![0_u8; 512];
    block[..name.len()].copy_from_slice(name.as_bytes());
    block[100..108].copy_from_slice(b"0000644\0");
    block[124..124 + size_field.len()].copy_from_slice(size_field);
    block[156] = kind;
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    block[148..156].copy_from_slice(b"        ");
    let sum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
    block[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    block
}

#[test]
fn a_huge_tar_extension_header_is_refused_before_it_is_read() {
    // A GNU long name declaring 8 GiB (octal 77777777777), with nothing
    // after: the reader would hold it whole in memory.
    for kind in [b'L', b'K', b'x', b'g'] {
        let dir = TempDir::new().unwrap();
        let archive = dir.path().join("long.tar");
        std::fs::write(
            &archive,
            tar_header("././@LongLink", kind, b"77777777777\0"),
        )
        .unwrap();
        let out = TempDir::new().unwrap();
        let error = unpack(&archive, ArchiveKind::Tar, out.path(), &limits()).unwrap_err();
        assert!(
            matches!(error, ArchiveError::Index("tar extension header")),
            "{}: {error}",
            kind as char
        );
        assert_eq!(error.code(), "ARCHIVE_TOO_LARGE");
    }
    // The same in base-256, as GNU writes sizes over 8 GiB.
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("long.tar");
    let mut size = vec![0x80_u8; 1];
    size.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0]);
    std::fs::write(&archive, tar_header("././@LongLink", b'L', &size)).unwrap();
    let out = TempDir::new().unwrap();
    assert!(matches!(
        unpack(&archive, ArchiveKind::Tar, out.path(), &limits()),
        Err(ArchiveError::Index(_))
    ));
}

/// One PAX record, `"<length> <key>=<value>\n"`, its length counting itself.
fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let body = format!(" {key}={value}\n");
    let mut length = body.len() + 1;
    while format!("{length}{body}").len() != length {
        length += 1;
    }
    format!("{length}{body}").into_bytes()
}

/// A PAX header block and its data, padded to whole blocks.
fn pax_header(records: &[u8]) -> Vec<u8> {
    let mut out = tar_header(
        "PaxHeaders/entry",
        b'x',
        format!("{:011o}\0", records.len()).as_bytes(),
    );
    let mut data = records.to_vec();
    data.resize(records.len().div_ceil(512) * 512, 0);
    out.extend(data);
    out
}

fn unpack_bytes(
    bytes: &[u8],
) -> Result<(TempDir, tangible_image::archive::Unpacked), ArchiveError> {
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("crafted.tar");
    std::fs::write(&archive, bytes).unwrap();
    let out = TempDir::new().unwrap();
    let unpacked = unpack(&archive, ArchiveKind::Tar, out.path(), &limits())?;
    drop(dir);
    Ok((out, unpacked))
}

#[test]
fn a_pax_size_does_not_put_the_guard_out_of_step() {
    // Found by the 2026-11-12 security review. A PAX size of 0 for an entry
    // whose own header says 1024: the reader skips nothing, and a guard
    // counting the header's 1024 would skip the next header, a GNU long
    // name past the cap, and let the reader hold it whole. 2 MiB here; the
    // same with gigabytes ran the server out of memory.
    let long_name: usize = 2 << 20;
    let mut tar = pax_header(&pax_record("size", "0"));
    tar.extend(tar_header("a", b'0', b"00000002000\0"));
    let mut long = tar_header(
        "././@LongLink",
        b'L',
        format!("{long_name:011o}\0").as_bytes(),
    );
    long[257..265].copy_from_slice(b"ustar  \0");
    tar.extend(long);
    tar.extend(std::iter::repeat_n(0_u8, long_name));
    tar.extend(tar_header("b", b'0', b"00000000000\0"));
    tar.extend(std::iter::repeat_n(0_u8, 1024));

    let error = unpack_bytes(&tar).unwrap_err();
    assert!(
        matches!(error, ArchiveError::Index("tar extension header")),
        "{error}"
    );
}

#[test]
fn a_pax_size_is_followed_as_the_reader_follows_it() {
    // How a tar records a file over 8 GiB: its header's size is a stand-in,
    // and a PAX record gives the real one. Here 1000 bytes behind a header
    // that says 0; the guard must skip what the reader reads.
    let data: Vec<u8> = (0..1000_u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let mut tar = pax_header(&pax_record("size", "1000"));
    tar.extend(tar_header("disc.bin", b'0', b"00000000000\0"));
    let mut padded = data.clone();
    padded.resize(1024, 0);
    tar.extend(padded);
    tar.extend(std::iter::repeat_n(0_u8, 1024));

    let (out, _) = unpack_bytes(&tar).unwrap();
    assert_eq!(std::fs::read(out.path().join("disc.bin")).unwrap(), data);
}

#[test]
fn two_pax_headers_for_one_entry_are_refused() {
    let mut tar = pax_header(&pax_record("size", "0"));
    tar.extend(pax_header(&pax_record("size", "512")));
    tar.extend(tar_header("a", b'0', b"00000000000\0"));
    tar.extend(std::iter::repeat_n(0_u8, 1536));
    assert!(unpack_bytes(&tar).is_err());
}

/// A 7z number of exactly eight bytes.
fn seven_zip_u64(value: u64) -> Vec<u8> {
    let mut bytes = vec![0xFF];
    bytes.extend_from_slice(&value.to_le_bytes());
    bytes
}

/// CRC-32 (IEEE), as 7z checksums its headers.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A 7z of nothing but `header`, its start header checksummed as 7-Zip
/// writes it.
fn seven_zip_file(header: &[u8]) -> Vec<u8> {
    let mut start = Vec::new();
    start.extend_from_slice(&0_u64.to_le_bytes());
    start.extend_from_slice(&(header.len() as u64).to_le_bytes());
    start.extend_from_slice(&crc32(header).to_le_bytes());
    let mut file = vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0x00, 0x04];
    file.extend_from_slice(&crc32(&start).to_le_bytes());
    file.extend_from_slice(&start);
    file.extend_from_slice(header);
    file
}

/// A 7z whose encoded header names one LZMA folder with `dictionary` and
/// decodes to `unpacked` bytes.
fn seven_zip_with_header(dictionary: u32, unpacked: u64) -> Vec<u8> {
    let mut header = vec![
        0x17, 0x06, 0x00, 0x01, 0x09, 0x0A, 0x00, 0x07, 0x0B, 0x01, 0x00,
    ];
    header.extend_from_slice(&[0x01, 0x23, 0x03, 0x01, 0x01, 0x05, 0x5D]);
    header.extend_from_slice(&dictionary.to_le_bytes());
    header.push(0x0C);
    header.extend_from_slice(&seven_zip_u64(unpacked));
    header.push(0x00);
    seven_zip_file(&header)
}

fn unpack_seven_zip(bytes: &[u8]) -> ArchiveError {
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("crafted.7z");
    std::fs::write(&archive, bytes).unwrap();
    let out = TempDir::new().unwrap();
    unpack(&archive, ArchiveKind::SevenZip, out.path(), &limits()).unwrap_err()
}

#[test]
fn a_7z_header_declaring_millions_of_files_is_refused_before_it_is_read() {
    // Found by the 2026-11-12 security review. The 7z reader makes room for
    // every file a header declares, over 100 bytes each, bounded only by the
    // header's length: a plain header of 5 MiB declaring five million files
    // would take over 500 MB, and one of 64 MiB about 7 GiB.
    let mut header = vec![0x01, 0x05];
    header.extend_from_slice(&seven_zip_u64(5_000_000));
    header.resize(5 << 20, 0);
    let error = unpack_seven_zip(&seven_zip_file(&header));
    assert!(matches!(error, ArchiveError::Index("header")), "{error}");

    // Encoded, the same: a header that would decode past the cap.
    let error = unpack_seven_zip(&seven_zip_with_header(1 << 16, 5 << 20));
    assert!(matches!(error, ArchiveError::Index("header")), "{error}");
}

#[test]
fn a_7z_with_as_many_entries_as_an_import_allows_unpacks() {
    // Made by 7-Zip: a folder and 9,999 files whose paths are 180
    // characters long. Its header decodes to 3.6 MiB, so the cap on a 7z's
    // header, which stops one declaring millions of files, must leave room
    // for it.
    let (dir, out) = unpacked("many.7z").unwrap();
    assert_eq!(out.files.len(), 9_999);
    assert_eq!(out.expanded_bytes, 9_999);
    let (first, length) = &out.files[0];
    assert_eq!(first.as_str().len(), 180);
    assert_eq!(*length, 1);
    assert_eq!(
        std::fs::read(dir.path().join(first.as_str())).unwrap(),
        b"x"
    );
}

#[test]
fn a_7z_without_its_start_header_is_refused_before_the_reader_guesses_one() {
    // A zero checksum and nothing after it sends the 7z reader looking
    // through the file's last megabyte for a header, which it then decodes
    // with no limit; Tangible could not check that header first.
    let mut file = vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0x00, 0x04];
    file.extend_from_slice(&[0; 24]);
    file.extend_from_slice(&[0x17, 0x06, 0x00, 0x01, 0x09, 0x0A, 0x00]);
    let error = unpack_seven_zip(&file);
    assert!(
        matches!(&error, ArchiveError::Corrupt(text) if text.contains("start header")),
        "{error}"
    );

    // And one whose start header does not match its checksum: the reader
    // would refuse it too, and Tangible must not read a different header
    // from the one the reader would.
    let mut file = seven_zip_with_header(1 << 16, 4096);
    file[12] ^= 0x01;
    let error = unpack_seven_zip(&file);
    assert!(
        matches!(&error, ArchiveError::Corrupt(text) if text.contains("start header")),
        "{error}"
    );
}

#[test]
fn a_7z_header_that_would_decode_to_gigabytes_is_refused_before_it_is() {
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("header.7z");
    std::fs::write(&archive, seven_zip_with_header(1 << 16, 64 << 30)).unwrap();
    let out = TempDir::new().unwrap();
    let error = unpack(&archive, ArchiveKind::SevenZip, out.path(), &limits()).unwrap_err();
    assert!(matches!(error, ArchiveError::Index("header")), "{error}");

    std::fs::write(&archive, seven_zip_with_header(2 << 30, 4096)).unwrap();
    let out = TempDir::new().unwrap();
    let error = unpack(&archive, ArchiveKind::SevenZip, out.path(), &limits()).unwrap_err();
    assert!(matches!(error, ArchiveError::Dictionary { .. }), "{error}");
}

#[test]
fn a_zip_directory_too_large_is_refused_before_it_is_read() {
    // Only an end record: what it claims is what is checked.
    let end = |entries: u16, size: u32| {
        let mut record = b"PK\x05\x06".to_vec();
        record.extend_from_slice(&[0, 0, 0, 0]);
        record.extend_from_slice(&entries.to_le_bytes());
        record.extend_from_slice(&entries.to_le_bytes());
        record.extend_from_slice(&size.to_le_bytes());
        record.extend_from_slice(&0_u32.to_le_bytes());
        record.extend_from_slice(&[0, 0]);
        record
    };
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("many.zip");
    std::fs::write(&archive, end(60_000, 1000)).unwrap();
    let out = TempDir::new().unwrap();
    assert!(matches!(
        unpack(&archive, ArchiveKind::Zip, out.path(), &limits()),
        Err(ArchiveError::TooManyEntries(_))
    ));
    std::fs::write(&archive, end(10, 200 << 20)).unwrap();
    let out = TempDir::new().unwrap();
    assert!(matches!(
        unpack(&archive, ArchiveKind::Zip, out.path(), &limits()),
        Err(ArchiveError::Index("central directory"))
    ));
}
