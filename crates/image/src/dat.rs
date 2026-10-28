// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Logiqx XML DATs: the format Redump and No-Intro publish their lists of
//! known good dumps in (docs/plans/dat-matching.md H2).
//!
//! A DAT is a `datafile` holding a `header` (its name, description, version,
//! author, homepage) and `game` elements (`machine` in MAME's), each naming
//! the files of one dump as `rom` elements with their size and CRC32, MD5
//! and SHA-1 (No-Intro sometimes SHA-256).
//!
//! It is untrusted input, uploaded by an administrator from wherever they
//! got it, and read with quick-xml, which never fetches a document type or
//! expands an entity beyond XML's five and character references: a DAT that
//! needs any other is refused. Everything is bounded: the file's size, how
//! deep elements nest, how many games and roms, how long a text. Hashes are
//! checked for shape and kept lowercase; a malformed one is left out and
//! warned about, since a wrong hash would only ever fail to match.

use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// Largest DAT read. Redump's largest are a few megabytes; No-Intro's tens.
pub const MAX_DAT_BYTES: usize = 64 << 20;

/// Most games one DAT may hold.
const MAX_GAMES: usize = 500_000;

/// Most roms one DAT may hold, all games together.
const MAX_ROMS: usize = 4_000_000;

/// Deepest elements nest. A Logiqx DAT goes four deep.
const MAX_DEPTH: usize = 16;

/// Longest text or attribute value kept.
const MAX_TEXT: usize = 2048;

/// A DAT's header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatHeader {
    /// Its name, such as `Sony - PlayStation`; what identifies the DAT, so
    /// a newer one of the same name replaces it.
    pub name: String,
    /// Its description, often the name with the date.
    pub description: Option<String>,
    /// Its version, often a date.
    pub version: Option<String>,
    /// Who made it.
    pub author: Option<String>,
    /// Where it is from.
    pub homepage: Option<String>,
    /// Its address.
    pub url: Option<String>,
}

/// One file of a dump.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatRom {
    /// Its file name, such as `Game (USA) (Track 1).bin`.
    pub name: String,
    /// Its size in bytes.
    pub size: Option<u64>,
    /// CRC32, eight lowercase hex digits.
    pub crc32: Option<String>,
    /// MD5.
    pub md5: Option<String>,
    /// SHA-1.
    pub sha1: Option<String>,
    /// SHA-256, where the DAT gives it.
    pub sha256: Option<String>,
    /// Its status, such as `verified` or `baddump`.
    pub status: Option<String>,
}

impl DatRom {
    /// Whether it is a CUE sheet, which identifies nothing: its bytes
    /// follow from the DAT's own file names, not the disc
    /// (INTEGRATIONS.md, "Traps"). Never a match key.
    #[must_use]
    pub fn is_cue(&self) -> bool {
        self.name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("cue"))
    }
}

/// One dump: a game and its files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatGame {
    /// Its name, such as `Game (USA)`.
    pub name: String,
    /// Its description.
    pub description: Option<String>,
    /// Its category, such as `Games` or `Demos`.
    pub category: Option<String>,
    /// Its files.
    pub roms: Vec<DatRom>,
}

/// Something in a DAT that does not stop it being read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DatWarning {
    /// A hash not of its algorithm's shape, left out.
    #[error("{game}: {rom}'s {algorithm} {value:?} is not well formed, and was left out")]
    MalformedHash {
        /// The game.
        game: String,
        /// The rom.
        rom: String,
        /// Which hash.
        algorithm: &'static str,
        /// What it said, cut short.
        value: String,
    },
    /// A size that is not a number, left out.
    #[error("{game}: {rom}'s size {value:?} is not a number, and was left out")]
    MalformedSize {
        /// The game.
        game: String,
        /// The rom.
        rom: String,
        /// What it said, cut short.
        value: String,
    },
    /// A game with no name, skipped.
    #[error("a game with no name was skipped")]
    UnnamedGame,
    /// A rom with no name, skipped.
    #[error("{0}: a file with no name was skipped")]
    UnnamedRom(String),
    /// Warnings past the first thousand, counted.
    #[error("and {0} more warnings")]
    More(usize),
}

/// Why a DAT could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DatError {
    /// Larger than any DAT.
    #[error("the file is larger than {MAX_DAT_BYTES} bytes")]
    TooLarge,
    /// Not well-formed XML, or XML this reader will not read (an entity it
    /// would have to expand).
    #[error("the file is not XML this reader will take, at byte {position}: {detail}")]
    Xml {
        /// Where.
        position: u64,
        /// What is wrong.
        detail: String,
    },
    /// Not a Logiqx `datafile`.
    #[error("the file is not a Logiqx DAT: its root element is not datafile")]
    NotLogiqx,
    /// No header name, which is what names the DAT.
    #[error("the DAT's header has no name")]
    NoName,
    /// Past a bound.
    #[error("the file holds more {0} than a DAT is allowed")]
    TooMany(&'static str),
}

/// A parsed DAT.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatFile {
    /// Its header.
    pub header: DatHeader,
    /// Its games, in the order given.
    pub games: Vec<DatGame>,
    /// What was seen that did not stop it being read.
    pub warnings: Vec<DatWarning>,
}

impl DatFile {
    /// How many roms in all.
    #[must_use]
    pub fn rom_count(&self) -> usize {
        self.games.iter().map(|game| game.roms.len()).sum()
    }
}

/// Text cut to the bound, without control characters.
fn bounded(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A hash of `digits` hex digits, lowercased, or why not.
fn checked_hash(value: &str, digits: usize) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    (value.len() == digits && value.bytes().all(|b| b.is_ascii_hexdigit())).then_some(value)
}

/// Where the reader is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Within {
    Nothing,
    Datafile,
    Header,
    Game,
}

/// The parse in progress.
struct Builder {
    file: DatFile,
    game: Option<DatGame>,
    rom_count: usize,
    dropped_warnings: usize,
    /// The element whose text is being read, in the header or a game.
    field: Option<String>,
    /// Its text so far: quick-xml hands text and each reference in it
    /// separately.
    field_text: String,
}

impl Builder {
    fn warn(&mut self, warning: DatWarning) {
        if self.file.warnings.len() < 1000 {
            self.file.warnings.push(warning);
        } else {
            self.dropped_warnings += 1;
        }
    }

    fn rom(&mut self, element: &BytesStart<'_>) -> Result<(), DatError> {
        let Some(game) = self.game.as_ref() else {
            return Ok(());
        };
        let game_name = game.name.clone();
        let mut rom = DatRom::default();
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|e| DatError::Xml {
                position: 0,
                detail: e.to_string(),
            })?;
            let value = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|e| DatError::Xml {
                    position: 0,
                    detail: e.to_string(),
                })?;
            let value = bounded(&value);
            match attribute.key.as_ref() {
                "name" => rom.name = value,
                "size" => match value.parse::<u64>() {
                    Ok(size) => rom.size = Some(size),
                    Err(_) => self.warn(DatWarning::MalformedSize {
                        game: game_name.clone(),
                        rom: rom.name.clone(),
                        value: value.chars().take(64).collect(),
                    }),
                },
                key @ ("crc" | "md5" | "sha1" | "sha256") => {
                    let (algorithm, digits) = match key {
                        "crc" => ("CRC32", 8),
                        "md5" => ("MD5", 32),
                        "sha1" => ("SHA-1", 40),
                        _ => ("SHA-256", 64),
                    };
                    match checked_hash(&value, digits) {
                        Some(hash) => match digits {
                            8 => rom.crc32 = Some(hash),
                            32 => rom.md5 = Some(hash),
                            40 => rom.sha1 = Some(hash),
                            _ => rom.sha256 = Some(hash),
                        },
                        None => self.warn(DatWarning::MalformedHash {
                            game: game_name.clone(),
                            rom: rom.name.clone(),
                            algorithm,
                            value: value.chars().take(64).collect(),
                        }),
                    }
                }
                "status" => rom.status = Some(value),
                _ => {}
            }
        }
        if rom.name.is_empty() {
            self.warn(DatWarning::UnnamedRom(game_name));
            return Ok(());
        }
        self.rom_count += 1;
        if self.rom_count > MAX_ROMS {
            return Err(DatError::TooMany("files"));
        }
        if let Some(game) = self.game.as_mut() {
            game.roms.push(rom);
        }
        Ok(())
    }

    /// Add to the text of the element being read, up to the bound.
    fn push_text(&mut self, text: &str) {
        if self.field.is_some() && self.field_text.len() < MAX_TEXT * 4 {
            self.field_text.push_str(text);
        }
    }

    /// The element being read has ended: keep its text.
    fn end_field(&mut self, within: Within) {
        let Some(field) = self.field.take() else {
            return;
        };
        let text = std::mem::take(&mut self.field_text);
        self.text(within, &field, &text);
    }

    fn text(&mut self, within: Within, field: &str, text: &str) {
        let text = bounded(text);
        if text.is_empty() {
            return;
        }
        match within {
            Within::Header => {
                let header = &mut self.file.header;
                let slot = match field {
                    "name" => {
                        header.name = text;
                        return;
                    }
                    "description" => &mut header.description,
                    "version" => &mut header.version,
                    "author" => &mut header.author,
                    "homepage" => &mut header.homepage,
                    "url" => &mut header.url,
                    _ => return,
                };
                *slot = Some(text);
            }
            Within::Game => {
                if let Some(game) = self.game.as_mut() {
                    match field {
                        "description" => game.description = Some(text),
                        "category" => game.category = Some(text),
                        _ => {}
                    }
                }
            }
            Within::Datafile | Within::Nothing => {}
        }
    }
}

/// What a reference in text stands for: one of XML's five entities or a
/// character. Any other entity would need a document type, which is never
/// read, so it is refused.
fn resolve_reference(reference: &BytesRef<'_>) -> Result<char, String> {
    if let Some(c) = reference.resolve_char_ref().map_err(|e| e.to_string())? {
        return Ok(c);
    }
    match reference.as_ref() {
        "lt" => Ok('<'),
        "gt" => Ok('>'),
        "amp" => Ok('&'),
        "apos" => Ok('\''),
        "quot" => Ok('"'),
        other => Err(format!(
            "the entity &{}; is not one this reader expands",
            other.chars().take(64).collect::<String>()
        )),
    }
}

/// The `name` attribute of an element.
fn name_attribute(element: &BytesStart<'_>) -> Result<Option<String>, DatError> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|e| DatError::Xml {
            position: 0,
            detail: e.to_string(),
        })?;
        if attribute.key.as_ref() == "name" {
            let value = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|e| DatError::Xml {
                    position: 0,
                    detail: e.to_string(),
                })?;
            return Ok(Some(bounded(&value)));
        }
    }
    Ok(None)
}

/// Read a Logiqx XML DAT.
///
/// # Errors
///
/// [`DatError`] for a file that is not one, is not XML this reader takes,
/// or is past the bounds.
#[allow(clippy::too_many_lines)]
pub fn parse(bytes: &[u8]) -> Result<DatFile, DatError> {
    if bytes.len() > MAX_DAT_BYTES {
        return Err(DatError::TooLarge);
    }
    let mut reader = Reader::from_reader(bytes);
    let mut builder = Builder {
        file: DatFile::default(),
        game: None,
        rom_count: 0,
        dropped_warnings: 0,
        field: None,
        field_text: String::new(),
    };
    let mut stack: Vec<String> = Vec::new();
    let mut within = Within::Nothing;
    let mut buffer = Vec::new();
    let mut saw_root = false;
    loop {
        let position = reader.buffer_position();
        let xml = |e: quick_xml::Error| DatError::Xml {
            position,
            detail: e.to_string(),
        };
        let event = reader.read_event_into(&mut buffer).map_err(xml)?;
        match event {
            Event::Start(element) => {
                let name = element.local_name().as_ref().to_owned();
                if stack.is_empty() {
                    if name != "datafile" {
                        return Err(DatError::NotLogiqx);
                    }
                    saw_root = true;
                    within = Within::Datafile;
                } else {
                    match (within, name.as_str()) {
                        (Within::Datafile, "header") => within = Within::Header,
                        (Within::Datafile, "game" | "machine") => {
                            within = Within::Game;
                            if builder.file.games.len() >= MAX_GAMES {
                                return Err(DatError::TooMany("games"));
                            }
                            match name_attribute(&element)? {
                                Some(game) if !game.is_empty() => {
                                    builder.game = Some(DatGame {
                                        name: game,
                                        ..DatGame::default()
                                    });
                                }
                                _ => builder.warn(DatWarning::UnnamedGame),
                            }
                        }
                        (Within::Game, "rom") => builder.rom(&element)?,
                        (Within::Header | Within::Game, field) => {
                            builder.field = Some(field.to_owned());
                            builder.field_text.clear();
                        }
                        _ => {}
                    }
                }
                stack.push(name);
                if stack.len() > MAX_DEPTH {
                    return Err(DatError::TooMany("levels of nesting"));
                }
            }
            Event::Empty(element) => {
                if stack.is_empty() {
                    return Err(DatError::NotLogiqx);
                }
                if within == Within::Game && element.local_name().as_ref() == "rom" {
                    builder.rom(&element)?;
                }
            }
            Event::Text(text) => {
                builder.push_text(&text.xml10_content());
            }
            Event::GeneralRef(reference) => {
                let c = resolve_reference(&reference)
                    .map_err(|detail| DatError::Xml { position, detail })?;
                builder.push_text(c.encode_utf8(&mut [0; 4]));
            }
            Event::CData(text) => {
                builder.push_text(text.as_ref());
            }
            Event::End(element) => {
                let name = element.local_name().as_ref().to_owned();
                stack.pop();
                builder.end_field(within);
                match (within, name.as_str()) {
                    (Within::Header, "header") => within = Within::Datafile,
                    (Within::Game, "game" | "machine") => {
                        within = Within::Datafile;
                        if let Some(game) = builder.game.take() {
                            builder.file.games.push(game);
                        }
                    }
                    (Within::Datafile, "datafile") => within = Within::Nothing,
                    _ => {}
                }
            }
            Event::Eof => break,
            // The XML declaration, comments, processing instructions and a
            // document type are read past; a document type's declarations
            // are never fetched or used.
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) | Event::DocType(_) => {}
        }
        buffer.clear();
    }
    if !saw_root {
        return Err(DatError::NotLogiqx);
    }
    if builder.file.header.name.is_empty() {
        return Err(DatError::NoName);
    }
    if builder.dropped_warnings > 0 {
        let more = builder.dropped_warnings;
        builder.file.warnings.push(DatWarning::More(more));
    }
    Ok(builder.file)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const REDUMP: &str = r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile>
	<header>
		<name>Example - Discs</name>
		<description>Example - Discs - Datfile (2) (2026-10-27 12-00-00)</description>
		<version>2026-10-27 12-00-00</version>
		<date>2026-10-27 12-00-00</date>
		<author>Tangible &amp; friends</author>
		<homepage>redump.org</homepage>
		<url>http://redump.org/</url>
	</header>
	<game name="Mixed Disc (World)">
		<category>Games</category>
		<description>Mixed Disc (World)</description>
		<rom name="Mixed Disc (World).cue" size="242" crc="0A1B2C3D" md5="00112233445566778899aabbccddeeff" sha1="00112233445566778899aabbccddeeff00112233"/>
		<rom name="Mixed Disc (World) (Track 1).bin" size="705600" crc="414FA339" md5="9E107D9D372BB6826BD81D3542A419D6" sha1="2FD4E1C67A2D28FCED849EE1BB76E7391B93EB12"/>
		<rom name="Mixed Disc (World) (Track 2).bin" size="352800" crc="deadbeef" md5="d41d8cd98f00b204e9800998ecf8427e" sha1="da39a3ee5e6b4b0d3255bfef95601890afd80709" status="verified"/>
	</game>
	<game name="Odd &lt;Name&gt; &#x26; &#65;">
		<description><![CDATA[In <CDATA>]]></description>
		<rom name="odd.iso" size="2048" sha1="not-a-hash" crc="12345678"/>
		<rom name="sized.iso" size="big"/>
		<rom size="1"/>
	</game>
</datafile>
"#;

    #[test]
    fn reads_a_redump_dat() {
        let dat = parse(REDUMP.as_bytes()).expect("parse");
        assert_eq!(dat.header.name, "Example - Discs");
        assert_eq!(dat.header.version.as_deref(), Some("2026-10-27 12-00-00"));
        assert_eq!(dat.header.author.as_deref(), Some("Tangible & friends"));
        assert_eq!(dat.header.homepage.as_deref(), Some("redump.org"));
        assert_eq!(dat.header.url.as_deref(), Some("http://redump.org/"));
        assert_eq!(dat.games.len(), 2);
        assert_eq!(dat.rom_count(), 5);

        let mixed = &dat.games[0];
        assert_eq!(mixed.name, "Mixed Disc (World)");
        assert_eq!(mixed.category.as_deref(), Some("Games"));
        assert_eq!(mixed.description.as_deref(), Some("Mixed Disc (World)"));
        assert!(mixed.roms[0].is_cue());
        assert!(!mixed.roms[1].is_cue());
        let track = &mixed.roms[1];
        assert_eq!(track.size, Some(705_600));
        // Hashes come back lowercase, whatever case the DAT used.
        assert_eq!(track.crc32.as_deref(), Some("414fa339"));
        assert_eq!(
            track.md5.as_deref(),
            Some("9e107d9d372bb6826bd81d3542a419d6")
        );
        assert_eq!(
            track.sha1.as_deref(),
            Some("2fd4e1c67a2d28fced849ee1bb76e7391b93eb12")
        );
        assert_eq!(mixed.roms[2].status.as_deref(), Some("verified"));
    }

    #[test]
    fn expands_xml_entities_and_character_references_and_reads_cdata() {
        let dat = parse(REDUMP.as_bytes()).expect("parse");
        let odd = &dat.games[1];
        assert_eq!(odd.name, "Odd <Name> & A");
        assert_eq!(odd.description.as_deref(), Some("In <CDATA>"));
    }

    #[test]
    fn leaves_out_malformed_hashes_and_sizes_and_unnamed_files_saying_so() {
        let dat = parse(REDUMP.as_bytes()).expect("parse");
        let odd = &dat.games[1];
        assert_eq!(odd.roms.len(), 2, "the file with no name is skipped");
        assert_eq!(odd.roms[0].sha1, None);
        assert_eq!(odd.roms[0].crc32.as_deref(), Some("12345678"));
        assert_eq!(odd.roms[1].size, None);
        assert_eq!(dat.warnings.len(), 3, "{:?}", dat.warnings);
        assert!(matches!(
            &dat.warnings[0],
            DatWarning::MalformedHash { algorithm: "SHA-1", rom, .. } if rom == "odd.iso"
        ));
        assert!(matches!(&dat.warnings[1], DatWarning::MalformedSize { .. }));
        assert!(
            matches!(&dat.warnings[2], DatWarning::UnnamedRom(game) if game == "Odd <Name> & A")
        );
    }

    #[test]
    fn reads_mame_machines_and_ignores_unknown_elements() {
        let dat = parse(
            br#"<datafile><header><name>M</name><clrmamepro forcenodump="required"/></header>
                <machine name="m1"><year>1990</year><rom name="a.bin" size="4" sha1="2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"/></machine>
                <game name="g1"><rom name="b.bin" size="4"></rom></game></datafile>"#,
        )
        .expect("parse");
        assert_eq!(dat.games.len(), 2);
        assert_eq!(dat.games[0].name, "m1");
        assert_eq!(dat.games[1].roms[0].name, "b.bin");
    }

    #[test]
    fn refuses_what_is_not_a_logiqx_dat() {
        assert_eq!(parse(b"").unwrap_err(), DatError::NotLogiqx);
        assert_eq!(
            parse(b"<softwarelist name=\"x\"/>").unwrap_err(),
            DatError::NotLogiqx
        );
        assert_eq!(
            parse(b"<html><body/></html>").unwrap_err(),
            DatError::NotLogiqx
        );
        assert_eq!(
            parse(b"<datafile><header></header></datafile>").unwrap_err(),
            DatError::NoName
        );
        assert!(matches!(
            parse(b"<datafile><header><name>x</header></datafile>").unwrap_err(),
            DatError::Xml { .. }
        ));
        assert!(matches!(
            parse(&[0xff, 0xfe, 0x00]).unwrap_err(),
            DatError::NotLogiqx | DatError::Xml { .. }
        ));
    }

    #[test]
    fn never_expands_an_entity_a_document_type_declares() {
        // A billion laughs, and an external entity: neither is expanded.
        let laughs = br#"<?xml version="1.0"?>
<!DOCTYPE datafile [
  <!ENTITY lol "lol">
  <!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
  <!ENTITY secret SYSTEM "file:///etc/passwd">
]>
<datafile><header><name>&lol2;</name></header></datafile>"#;
        let error = parse(laughs).unwrap_err();
        assert!(
            matches!(&error, DatError::Xml { detail, .. } if detail.contains("&lol2;")),
            "{error:?}"
        );
        let external = br#"<!DOCTYPE datafile [<!ENTITY secret SYSTEM "file:///etc/passwd">]>
<datafile><header><name>x</name><author>&secret;</author></header></datafile>"#;
        assert!(matches!(parse(external).unwrap_err(), DatError::Xml { .. }));
    }

    #[test]
    fn bounds_size_depth_and_text() {
        assert_eq!(
            parse(&vec![b' '; MAX_DAT_BYTES + 1]).unwrap_err(),
            DatError::TooLarge
        );
        let deep = format!(
            "<datafile><header><name>x</name></header><game name=\"g\">{}{}</game></datafile>",
            "<a>".repeat(MAX_DEPTH),
            "</a>".repeat(MAX_DEPTH)
        );
        assert_eq!(
            parse(deep.as_bytes()).unwrap_err(),
            DatError::TooMany("levels of nesting")
        );
        let long = format!(
            "<datafile><header><name>{}</name></header></datafile>",
            "n".repeat(MAX_TEXT * 10)
        );
        let dat = parse(long.as_bytes()).expect("parse");
        assert_eq!(dat.header.name.len(), MAX_TEXT);
    }

    #[test]
    fn keeps_control_characters_out_of_names() {
        let dat = parse(b"<datafile><header><name>a&#x9;b&#x1b;[31mc</name></header></datafile>")
            .expect("parse");
        assert_eq!(dat.header.name, "ab[31mc");
    }

    #[test]
    fn counts_warnings_past_a_thousand() {
        let mut xml = String::from("<datafile><header><name>x</name></header><game name=\"g\">");
        for _ in 0..1005 {
            xml.push_str("<rom size=\"1\"/>");
        }
        xml.push_str("</game></datafile>");
        let dat = parse(xml.as_bytes()).expect("parse");
        assert_eq!(dat.warnings.len(), 1001);
        assert_eq!(dat.warnings[1000], DatWarning::More(5));
    }
}
