// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A torrent's identity, from a `.torrent` file or a magnet link an operator
//! supplied (docs/plans/acquisition.md A4).
//!
//! The infohash is how Tangible follows a torrent in a download client: names
//! are never trusted, and a torrent moved or renamed in the client keeps its
//! hash. It is computed here, before the client is asked to add anything, so
//! a submission is identified even when the client already had it.
//!
//! Only as much bencode is read as identity needs: the top-level dictionary
//! is walked to find the raw bytes of `info`, and `info`'s own keys are
//! looked at for `pieces` and `meta version`. Nothing else is decoded or
//! kept. The file is attacker-controlled, so the walk is bounded in size and
//! depth, checks every length against what is left, and refuses rather than
//! guesses.
//!
//! The hash is the one qBittorrent reports as `hash`: SHA-1 of `info` for a
//! v1 or hybrid torrent, and the first 20 bytes of its SHA-256 for a v2-only
//! one (BEP 52).

use std::fmt;

use sha2::{Digest as _, Sha256};

/// The largest `.torrent` accepted. Large enough for a torrent of several
/// hundred gigabytes at small piece sizes.
pub const MAX_TORRENT_BYTES: usize = 8 * 1024 * 1024;

/// The longest magnet link accepted.
pub const MAX_MAGNET_CHARS: usize = 8192;

/// How deeply lists and dictionaries may nest. Real torrents nest a handful
/// of levels; the bound is what keeps a crafted file from exhausting the
/// stack.
const MAX_DEPTH: usize = 32;

/// A torrent's infohash: 40 lowercase hex digits.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Infohash(String);

impl Infohash {
    /// One already in hand, such as stored, if it is 40 hex digits.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        (text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| Self(text.to_ascii_lowercase()))
    }

    /// As 40 lowercase hex digits.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Infohash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Infohash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Infohash({})", self.0)
    }
}

/// Why a torrent or magnet link was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TorrentError {
    /// Larger than [`MAX_TORRENT_BYTES`].
    #[error("a .torrent file is at most {MAX_TORRENT_BYTES} bytes")]
    TooLarge,
    /// Not well-formed bencode, or not a torrent.
    #[error("not a usable .torrent file: {0}")]
    Malformed(&'static str),
    /// Not a magnet link Tangible can follow.
    #[error("not a usable magnet link: {0}")]
    Magnet(&'static str),
}

/// The infohash of a `.torrent` file.
///
/// # Errors
///
/// [`TorrentError`] saying what is wrong with it.
pub fn infohash_of_torrent(bytes: &[u8]) -> Result<Infohash, TorrentError> {
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err(TorrentError::TooLarge);
    }
    if bytes.first() != Some(&b'd') {
        return Err(TorrentError::Malformed("it is not a dictionary"));
    }
    let mut info = None;
    let mut position = 1;
    loop {
        match bytes.get(position) {
            Some(b'e') => break,
            Some(_) => {}
            None => return Err(TorrentError::Malformed("it ends early")),
        }
        let (key, after_key) = string_at(bytes, position)?;
        let end = skip(bytes, after_key, 1)?;
        if key == b"info" {
            if info.is_some() {
                return Err(TorrentError::Malformed("it has two info dictionaries"));
            }
            info = Some((after_key, end));
        }
        position = end;
    }
    let (start, end) = info.ok_or(TorrentError::Malformed("it has no info dictionary"))?;
    let raw = &bytes[start..end];
    if raw.first() != Some(&b'd') {
        return Err(TorrentError::Malformed("its info is not a dictionary"));
    }
    let shape = info_shape(raw)?;
    let digest = if shape.has_pieces {
        sha1_of(raw)
    } else if shape.meta_version == Some(2) {
        Sha256::digest(raw)[..20].to_vec()
    } else {
        return Err(TorrentError::Malformed(
            "its info dictionary has neither pieces nor meta version 2",
        ));
    };
    Ok(Infohash(hex::encode(digest)))
}

/// SHA-1, for v1 infohashes only. Never used for identity inside Tangible
/// (AGENTS.md 4.1): it names the torrent the way the client does.
fn sha1_of(bytes: &[u8]) -> Vec<u8> {
    <sha1::Sha1 as sha1::Digest>::digest(bytes).to_vec()
}

/// What `info` says about which hash names the torrent.
struct InfoShape {
    has_pieces: bool,
    meta_version: Option<i64>,
}

fn info_shape(raw: &[u8]) -> Result<InfoShape, TorrentError> {
    let mut shape = InfoShape {
        has_pieces: false,
        meta_version: None,
    };
    let mut position = 1;
    loop {
        match raw.get(position) {
            Some(b'e') => break,
            Some(_) => {}
            None => return Err(TorrentError::Malformed("its info ends early")),
        }
        let (key, after_key) = string_at(raw, position)?;
        let end = skip(raw, after_key, 2)?;
        match key {
            b"pieces" => shape.has_pieces = true,
            b"meta version" => shape.meta_version = Some(integer_at(raw, after_key)?.0),
            _ => {}
        }
        position = end;
    }
    Ok(shape)
}

/// Where the value starting at `position` ends.
fn skip(bytes: &[u8], position: usize, depth: usize) -> Result<usize, TorrentError> {
    if depth > MAX_DEPTH {
        return Err(TorrentError::Malformed("it nests too deeply"));
    }
    match bytes.get(position) {
        Some(b'i') => integer_at(bytes, position).map(|(_, end)| end),
        Some(b'l') => {
            let mut at = position + 1;
            while bytes.get(at) != Some(&b'e') {
                if at >= bytes.len() {
                    return Err(TorrentError::Malformed("a list ends early"));
                }
                at = skip(bytes, at, depth + 1)?;
            }
            Ok(at + 1)
        }
        Some(b'd') => {
            let mut at = position + 1;
            while bytes.get(at) != Some(&b'e') {
                if at >= bytes.len() {
                    return Err(TorrentError::Malformed("a dictionary ends early"));
                }
                let (_, after_key) = string_at(bytes, at)?;
                at = skip(bytes, after_key, depth + 1)?;
            }
            Ok(at + 1)
        }
        Some(b'0'..=b'9') => string_at(bytes, position).map(|(_, end)| end),
        Some(_) => Err(TorrentError::Malformed(
            "it holds something that is not bencode",
        )),
        None => Err(TorrentError::Malformed("it ends early")),
    }
}

/// The byte string at `position`, and where it ends.
fn string_at(bytes: &[u8], position: usize) -> Result<(&[u8], usize), TorrentError> {
    let rest = bytes
        .get(position..)
        .ok_or(TorrentError::Malformed("it ends early"))?;
    let colon = rest
        .iter()
        .take(11)
        .position(|&b| b == b':')
        .ok_or(TorrentError::Malformed("a string has no usable length"))?;
    let digits = &rest[..colon];
    if digits.is_empty()
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits.len() > 1 && digits[0] == b'0')
    {
        return Err(TorrentError::Malformed("a string has no usable length"));
    }
    let length: usize = std::str::from_utf8(digits)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or(TorrentError::Malformed("a string has no usable length"))?;
    let start = position + colon + 1;
    let end = start
        .checked_add(length)
        .filter(|&end| end <= bytes.len())
        .ok_or(TorrentError::Malformed("a string runs past the end"))?;
    Ok((&bytes[start..end], end))
}

/// The integer at `position`, and where it ends.
fn integer_at(bytes: &[u8], position: usize) -> Result<(i64, usize), TorrentError> {
    if bytes.get(position) != Some(&b'i') {
        return Err(TorrentError::Malformed("an integer was expected"));
    }
    let rest = &bytes[position + 1..];
    let close = rest
        .iter()
        .take(21)
        .position(|&b| b == b'e')
        .ok_or(TorrentError::Malformed("an integer does not end"))?;
    let digits = std::str::from_utf8(&rest[..close])
        .map_err(|_| TorrentError::Malformed("an integer is not a number"))?;
    let canonical =
        digits == "0" || (!digits.starts_with('0') && !digits.starts_with("-0") && digits != "-");
    if digits.is_empty() || !canonical {
        return Err(TorrentError::Malformed("an integer is not a number"));
    }
    let value = digits
        .parse()
        .map_err(|_| TorrentError::Malformed("an integer is not a number"))?;
    Ok((value, position + 1 + close + 1))
}

/// The infohash a magnet link names, and the link as it may be handed to a
/// client.
///
/// A v1 `urn:btih:` (40 hex digits, or 32 base32) is used when present;
/// otherwise a v2 `urn:btmh:` (SHA-256 multihash, `1220` and 64 hex digits)
/// gives the truncated hash qBittorrent reports.
///
/// # Errors
///
/// [`TorrentError::Magnet`] saying what is wrong with it.
pub fn infohash_of_magnet(link: &str) -> Result<Infohash, TorrentError> {
    if link.len() > MAX_MAGNET_CHARS {
        return Err(TorrentError::Magnet("it is too long"));
    }
    if link.chars().any(char::is_control) {
        return Err(TorrentError::Magnet("it holds control characters"));
    }
    let query = link
        .strip_prefix("magnet:?")
        .ok_or(TorrentError::Magnet("it does not start with magnet:?"))?;
    let mut v2 = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if !(key == "xt" || key.starts_with("xt.")) {
            continue;
        }
        if let Some(hash) = value.strip_prefix("urn:btih:") {
            return v1_hash(hash);
        }
        if let Some(hash) = value.strip_prefix("urn:btmh:") {
            v2 = Some(v2_hash(hash)?);
        }
    }
    v2.ok_or(TorrentError::Magnet("it names no BitTorrent infohash"))
}

fn v1_hash(text: &str) -> Result<Infohash, TorrentError> {
    if let Some(hash) = Infohash::parse(text) {
        return Ok(hash);
    }
    if text.len() == 32 {
        return base32(text)
            .map(|bytes| Infohash(hex::encode(bytes)))
            .ok_or(TorrentError::Magnet("its btih is not base32"));
    }
    Err(TorrentError::Magnet(
        "its btih is not 40 hex or 32 base32 characters",
    ))
}

fn v2_hash(text: &str) -> Result<Infohash, TorrentError> {
    let digest = text
        .strip_prefix("1220")
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or(TorrentError::Magnet("its btmh is not a SHA-256 multihash"))?;
    Ok(Infohash(digest[..40].to_ascii_lowercase()))
}

/// Decode 32 characters of RFC 4648 base32 into 20 bytes.
fn base32(text: &str) -> Option<[u8; 20]> {
    let mut out = [0_u8; 20];
    let mut buffer = 0_u64;
    let mut bits = 0;
    let mut index = 0;
    for c in text.bytes() {
        let value = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | u64::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            *out.get_mut(index)? = u8::try_from((buffer >> bits) & 0xff).ok()?;
            index += 1;
        }
    }
    (index == 20).then_some(out)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A minimal bencoder, for building test torrents.
    enum B<'a> {
        I(i64),
        S(&'a [u8]),
        L(Vec<B<'a>>),
        D(Vec<(&'a [u8], B<'a>)>),
    }

    fn encode(value: &B<'_>) -> Vec<u8> {
        match value {
            B::I(n) => format!("i{n}e").into_bytes(),
            B::S(s) => [format!("{}:", s.len()).into_bytes(), s.to_vec()].concat(),
            B::L(items) => [
                b"l".to_vec(),
                items.iter().flat_map(encode).collect(),
                b"e".to_vec(),
            ]
            .concat(),
            B::D(entries) => [
                b"d".to_vec(),
                entries
                    .iter()
                    .flat_map(|(k, v)| [encode(&B::S(k)), encode(v)].concat())
                    .collect(),
                b"e".to_vec(),
            ]
            .concat(),
        }
    }

    fn v1_info() -> B<'static> {
        B::D(vec![
            (b"length", B::I(300_000)),
            (b"name", B::S(b"disc.iso")),
            (b"piece length", B::I(16384)),
            (b"pieces", B::S(&[7; 20])),
            (b"private", B::I(1)),
        ])
    }

    #[test]
    fn a_v1_torrent_is_named_by_the_sha1_of_its_info() {
        let info = encode(&v1_info());
        let torrent = encode(&B::D(vec![
            (b"announce", B::S(b"http://tracker.invalid/")),
            (b"info", v1_info()),
            (b"url-list", B::L(vec![B::S(b"x")])),
        ]));
        let expected = hex::encode(sha1_of(&info));
        assert_eq!(infohash_of_torrent(&torrent).unwrap().as_str(), expected);
    }

    #[test]
    fn a_v2_only_torrent_is_named_by_its_truncated_sha256() {
        let info = B::D(vec![
            (b"file tree", B::D(vec![])),
            (b"meta version", B::I(2)),
            (b"name", B::S(b"disc.iso")),
        ]);
        let raw = encode(&info);
        let torrent = encode(&B::D(vec![(b"info", info)]));
        assert_eq!(
            infohash_of_torrent(&torrent).unwrap().as_str(),
            hex::encode(&Sha256::digest(&raw)[..20])
        );
    }

    #[test]
    fn the_hashes_match_what_qbittorrent_reported() {
        // Private, trackerless torrents of random files, added to
        // qBittorrent 5.1.2 in a throwaway container; its torrents/info
        // answer is torrents-info-5.1.2.json beside them.
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/tool-output/qbittorrent"
        );
        let reported: serde_json::Value = serde_json::from_slice(
            &std::fs::read(format!("{fixtures}/torrents-info-5.1.2.json")).unwrap(),
        )
        .unwrap();
        let hash_of = |name: &str| {
            reported
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .unwrap()["hash"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        for (file, name) in [
            ("single-file.torrent", "probe-disc.iso"),
            ("multi-file.torrent", "Probe Disc (CUE)"),
        ] {
            let bytes = std::fs::read(format!("{fixtures}/{file}")).unwrap();
            assert_eq!(
                infohash_of_torrent(&bytes).unwrap().as_str(),
                hash_of(name),
                "{file}"
            );
        }
    }

    #[test]
    fn malformed_torrents_are_refused_saying_why() {
        let good = encode(&B::D(vec![(b"info", v1_info())]));
        let mut cases: Vec<(Vec<u8>, &str)> = vec![
            (Vec::new(), "not a dictionary"),
            (b"le".to_vec(), "not a dictionary"),
            (b"d".to_vec(), "ends early"),
            (b"de".to_vec(), "no info"),
            (good[..good.len() - 1].to_vec(), "ends early"),
            (b"d4:infoi1ee".to_vec(), "not a dictionary"),
            (b"d4:infod4:name1:xee".to_vec(), "neither pieces"),
            (b"d9999999999:xe".to_vec(), "runs past"),
            (b"d4:info".to_vec(), "ends early"),
            (b"di1ei2ee".to_vec(), "usable length"),
            (b"d04:infoe".to_vec(), "usable length"),
            (b"d1:ai01ee".to_vec(), "not a number"),
            (b"d1:ai-0ee".to_vec(), "not a number"),
            (b"d1:aiee".to_vec(), "not a number"),
            (b"d1:ax".to_vec(), "not bencode"),
            (
                encode(&B::D(vec![(b"info", v1_info()), (b"info", v1_info())])),
                "two info",
            ),
        ];
        let mut deep = b"d1:a".to_vec();
        deep.extend(std::iter::repeat_n(b'l', 100));
        deep.extend(std::iter::repeat_n(b'e', 100));
        deep.push(b'e');
        cases.push((deep, "too deeply"));
        for (bytes, says) in cases {
            let error = infohash_of_torrent(&bytes).expect_err("refused");
            assert!(
                error.to_string().contains(says),
                "{:?}: {error}",
                String::from_utf8_lossy(&bytes)
            );
        }
        assert_eq!(
            infohash_of_torrent(&vec![b'd'; MAX_TORRENT_BYTES + 1]),
            Err(TorrentError::TooLarge)
        );
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // A cheap stand-in for the fuzz target: every prefix and every
        // single-byte corruption of a real torrent is answered, not panicked
        // on.
        let good = encode(&B::D(vec![(b"info", v1_info())]));
        for end in 0..good.len() {
            let _ = infohash_of_torrent(&good[..end]);
        }
        for at in 0..good.len() {
            for byte in [0_u8, b'e', b'd', b'l', b'i', b':', b'9', 0xff] {
                let mut bad = good.clone();
                bad[at] = byte;
                let _ = infohash_of_torrent(&bad);
            }
        }
    }

    #[test]
    fn a_magnet_names_its_v1_hash_in_hex_or_base32_or_its_v2_hash() {
        let hex = "23760839656B18C262361C3019426C1E717B7310";
        assert_eq!(
            infohash_of_magnet(&format!("magnet:?xt=urn:btih:{hex}&dn=x"))
                .unwrap()
                .as_str(),
            hex.to_ascii_lowercase()
        );
        // The same 20 bytes in base32.
        let bytes = hex::decode(hex).unwrap();
        let encoded = base32_encode(&bytes);
        assert_eq!(
            infohash_of_magnet(&format!("magnet:?dn=x&xt=urn:btih:{encoded}"))
                .unwrap()
                .as_str(),
            hex.to_ascii_lowercase()
        );
        let v2 = "a".repeat(64);
        assert_eq!(
            infohash_of_magnet(&format!("magnet:?xt=urn:btmh:1220{v2}"))
                .unwrap()
                .as_str(),
            "a".repeat(40)
        );
        // A hybrid link: the v1 hash wins, as qBittorrent's `hash` does.
        assert_eq!(
            infohash_of_magnet(&format!("magnet:?xt=urn:btmh:1220{v2}&xt=urn:btih:{hex}"))
                .unwrap()
                .as_str(),
            hex.to_ascii_lowercase()
        );
    }

    #[test]
    fn unusable_magnets_are_refused_saying_why() {
        for (link, says) in [
            ("http://example.com/", "magnet:?"),
            ("magnet:?dn=x", "no BitTorrent"),
            ("magnet:?xt=urn:btih:abc", "40 hex"),
            (
                "magnet:?xt=urn:btih:00000000000000000000000000000001",
                "base32",
            ),
            ("magnet:?xt=urn:btmh:1114abcd", "multihash"),
            ("magnet:?xt=urn:btih:\n", "control"),
        ] {
            let error = infohash_of_magnet(link).expect_err("refused");
            assert!(error.to_string().contains(says), "{link}: {error}");
        }
        let long = format!(
            "magnet:?xt=urn:btih:{}&dn={}",
            "a".repeat(40),
            "x".repeat(9000)
        );
        assert!(infohash_of_magnet(&long).is_err());
    }

    fn base32_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut out = String::new();
        let mut buffer = 0_u32;
        let mut bits = 0;
        for &byte in bytes {
            buffer = (buffer << 8) | u32::from(byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
            }
        }
        out
    }
}
