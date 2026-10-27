// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Finding exported games among RomM's ROMs, and saying whether RomM's files
//! agree with what was written (docs/plans/romm-api.md M2).
//!
//! An export is a folder, `platform/game`, and RomM makes each game folder a
//! ROM whose `fs_name` is the folder's name. A ROM is looked for by the id
//! last found for it (a quick scan keeps it across a rename when the files
//! hash the same, as a renamed ISO's do and a renamed CUE/BIN's, whose
//! sheet changes, do not); then by RomM's search, which is fuzzy, so a
//! match must have the platform folder and name exactly; then, failing
//! that, by walking the platform's ROMs, at most [`MAX_WALK`] of them, once
//! per platform per pass.

use std::collections::{BTreeMap, HashMap};

use tangible_db::romm::ExportedFile;
use tangible_integrations::download::ClientError;
use tangible_integrations::romm::MARKER;
use tangible_integrations::romm_api::{MAX_PAGE, Rom, RommClient};

/// Most of a platform's ROMs walked through looking for one.
pub const MAX_WALK: u32 = 5000;

/// What RomM made of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// RomM has no ROM for the folder yet.
    Unseen,
    /// It has, and its files agree with what was written.
    Linked,
    /// RomM marks the ROM missing from its library.
    Missing,
    /// RomM's files do not agree with what was written.
    Differs,
}

impl LinkState {
    /// The stored word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unseen => "unseen",
            Self::Linked => "linked",
            Self::Missing => "missing",
            Self::Differs => "differs",
        }
    }
}

/// What looking an export up found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// What RomM made of it.
    pub state: LinkState,
    /// RomM's id for the ROM; none when unseen.
    pub rom_id: Option<i64>,
    /// What there is to say.
    pub detail: Option<String>,
}

impl Link {
    /// RomM has no ROM for the export, and why.
    #[must_use]
    pub fn unseen(detail: String) -> Self {
        Self {
            state: LinkState::Unseen,
            rom_id: None,
            detail: Some(detail),
        }
    }
}

/// Where a search for a folder ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// The folder's ROM.
    Rom(Box<Rom>),
    /// RomM has no platform for the folder's platform folder.
    NoPlatform,
    /// No ROM has the folder, of all the platform's or of the first
    /// [`MAX_WALK`].
    NotFound {
        /// Whether every one of the platform's ROMs was looked through.
        looked_through_all: bool,
    },
}

/// Looks folders up in one RomM, keeping what one pass has already read.
pub struct Finder<'a> {
    client: &'a RommClient,
    platforms: Option<HashMap<String, i64>>,
    walks: HashMap<i64, (Vec<Rom>, bool)>,
}

impl<'a> Finder<'a> {
    /// A finder asking `client`.
    #[must_use]
    pub fn new(client: &'a RommClient) -> Self {
        Self {
            client,
            platforms: None,
            walks: HashMap::new(),
        }
    }

    /// Find the ROM for `folder`, `platform/game`, trying `known` first.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn find(&mut self, folder: &str, known: Option<i64>) -> Result<Found, ClientError> {
        let Some((platform, name)) = folder.split_once('/') else {
            return Ok(Found::NotFound {
                looked_through_all: true,
            });
        };
        let holds = |rom: &Rom| rom.platform_fs_slug == platform && rom.fs_name == name;
        if let Some(id) = known
            && let Some(rom) = self.client.rom(id).await?
            && holds(&rom)
        {
            return Ok(Found::Rom(Box::new(rom)));
        }
        let Some(platform_id) = self.platform_id(platform).await? else {
            return Ok(Found::NoPlatform);
        };
        let page = self
            .client
            .roms(platform_id, Some(name), 0, MAX_PAGE)
            .await?;
        if let Some(rom) = page.items.into_iter().find(|rom| holds(rom)) {
            return Ok(Found::Rom(Box::new(rom)));
        }
        let (roms, complete) = self.walk(platform_id).await?;
        Ok(roms.iter().find(|rom| holds(rom)).map_or(
            Found::NotFound {
                looked_through_all: *complete,
            },
            |rom| Found::Rom(Box::new(rom.clone())),
        ))
    }

    async fn platform_id(&mut self, fs_slug: &str) -> Result<Option<i64>, ClientError> {
        if self.platforms.is_none() {
            let platforms = self.client.platforms().await?;
            self.platforms = Some(
                platforms
                    .into_iter()
                    .map(|platform| (platform.fs_slug, platform.id))
                    .collect(),
            );
        }
        Ok(self
            .platforms
            .as_ref()
            .and_then(|platforms| platforms.get(fs_slug).copied()))
    }

    /// The platform's ROMs, oldest first, at most [`MAX_WALK`], and whether
    /// that was all of them.
    async fn walk(&mut self, platform_id: i64) -> Result<&(Vec<Rom>, bool), ClientError> {
        if !self.walks.contains_key(&platform_id) {
            let mut roms = Vec::new();
            let mut offset = 0_u32;
            let complete = loop {
                let page = self
                    .client
                    .roms(platform_id, None, offset, MAX_PAGE)
                    .await?;
                let read = u32::try_from(page.items.len()).unwrap_or(u32::MAX);
                roms.extend(page.items);
                if read < MAX_PAGE {
                    break true;
                }
                offset = offset.saturating_add(read);
                if offset >= MAX_WALK {
                    break false;
                }
            };
            self.walks.insert(platform_id, (roms, complete));
        }
        Ok(&self.walks[&platform_id])
    }
}

/// What RomM made of a current export whose folder is `folder`, from where
/// the search ended and the files the export wrote.
#[must_use]
pub fn current_link(folder: &str, found: &Found, exported: &[ExportedFile]) -> Link {
    match found {
        Found::Rom(rom) if rom.missing_from_fs => Link {
            state: LinkState::Missing,
            rom_id: Some(rom.id),
            detail: Some(
                "RomM marks it missing from its library; a scan of its platform finds it again"
                    .to_owned(),
            ),
        },
        Found::Rom(rom) => {
            let differences = differences(rom, exported);
            Link {
                state: if differences.is_empty() {
                    LinkState::Linked
                } else {
                    LinkState::Differs
                },
                rom_id: Some(rom.id),
                detail: (!differences.is_empty()).then(|| differences.join("; ")),
            }
        }
        Found::NoPlatform => Link::unseen(format!(
            "RomM has no {} platform yet; a scan of its folder makes one",
            folder.split('/').next().unwrap_or(folder)
        )),
        Found::NotFound { looked_through_all } => Link::unseen(if *looked_through_all {
            format!("RomM has no game in {folder} yet; a scan of its platform finds it")
        } else {
            format!(
                "RomM has no game in {folder} among the first {MAX_WALK} of its platform's \
                 games, nor by searching for it"
            )
        }),
    }
}

/// What RomM made of an export that was taken out, from the ROM last found
/// for it: `None` when RomM no longer has the ROM, so there is nothing to
/// show.
#[must_use]
pub fn removed_link(rom: Option<&Rom>) -> Option<Link> {
    let rom = rom?;
    Some(if rom.missing_from_fs {
        Link {
            state: LinkState::Missing,
            rom_id: Some(rom.id),
            detail: Some("taken out of RomM's library, and RomM marks it missing".to_owned()),
        }
    } else {
        Link {
            state: LinkState::Linked,
            rom_id: Some(rom.id),
            detail: Some(
                "taken out of RomM's library, but RomM lists it until a scan of its platform"
                    .to_owned(),
            ),
        }
    })
}

/// How RomM's files for a ROM differ from those the export wrote: files
/// RomM lacks, files it has that the export did not write, and sizes that
/// disagree where the export recorded one. The export's marker, which RomM
/// lists unless told not to, is not counted.
#[must_use]
pub fn differences(rom: &Rom, exported: &[ExportedFile]) -> Vec<String> {
    let listed: BTreeMap<&str, i64> = rom
        .files
        .iter()
        .filter(|file| !file.file_name.eq_ignore_ascii_case(MARKER))
        .map(|file| (file.file_name.as_str(), file.file_size_bytes))
        .collect();
    let written: BTreeMap<&str, Option<i64>> = exported
        .iter()
        .map(|file| (file.name.as_str(), file.length_bytes))
        .collect();

    let lacking: Vec<&str> = written
        .keys()
        .filter(|name| !listed.contains_key(*name))
        .copied()
        .collect();
    let extra: Vec<&str> = listed
        .keys()
        .filter(|name| !written.contains_key(*name))
        .copied()
        .collect();
    let mut out = Vec::new();
    if !lacking.is_empty() {
        out.push(format!(
            "RomM lacks {} of the {} files written: {}",
            lacking.len(),
            written.len(),
            some_of(&lacking)
        ));
    }
    if !extra.is_empty() {
        out.push(format!(
            "RomM lists {} file(s) the export did not write: {}",
            extra.len(),
            some_of(&extra)
        ));
    }
    for (name, length) in &written {
        if let (Some(length), Some(seen)) = (length, listed.get(name))
            && length != seen
        {
            out.push(format!(
                "{name} is {seen} bytes in RomM and {length} as written"
            ));
        }
    }
    out
}

/// Up to three names, and how many more.
fn some_of(names: &[&str]) -> String {
    let shown = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
    if names.len() > 3 {
        format!("{shown} and {} more", names.len() - 3)
    } else {
        shown
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tangible_integrations::romm_api::{RomFile, parse_rom_page};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../../fixtures/tool-output/romm/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// The two-file game RomM 5.4.1 listed.
    fn probe_game() -> Rom {
        parse_rom_page(&fixture("roms-search-5.4.1.json"))
            .unwrap()
            .items
            .remove(0)
    }

    fn written(files: &[(&str, Option<i64>)]) -> Vec<ExportedFile> {
        files
            .iter()
            .map(|(name, length)| ExportedFile {
                name: (*name).to_owned(),
                length_bytes: *length,
            })
            .collect()
    }

    const FOLDER: &str = "psx/Probe Game (USA)";

    #[test]
    fn a_rom_whose_files_agree_is_linked() {
        let rom = probe_game();
        let exported = written(&[
            ("Probe Game (Disc 1).bin", Some(2_408_448)),
            ("Probe Game (Disc 1).cue", Some(85)),
        ]);
        let link = current_link(FOLDER, &Found::Rom(Box::new(rom.clone())), &exported);
        assert_eq!(
            link,
            Link {
                state: LinkState::Linked,
                rom_id: Some(rom.id),
                detail: None,
            }
        );
        // Receipts from before sizes were recorded compare by name.
        let without_sizes = written(&[
            ("Probe Game (Disc 1).bin", None),
            ("Probe Game (Disc 1).cue", None),
        ]);
        assert!(differences(&rom, &without_sizes).is_empty());
    }

    #[test]
    fn the_marker_romm_lists_is_not_a_difference() {
        let mut rom = probe_game();
        rom.files.push(RomFile {
            file_name: ".Tangible-Export.JSON".to_owned(),
            file_path: "roms/psx/Probe Game (USA)".to_owned(),
            file_size_bytes: 300,
        });
        let exported = written(&[
            ("Probe Game (Disc 1).bin", Some(2_408_448)),
            ("Probe Game (Disc 1).cue", Some(85)),
        ]);
        assert!(differences(&rom, &exported).is_empty());
    }

    #[test]
    fn what_differs_is_said() {
        let rom = probe_game();
        let exported = written(&[
            ("Probe Game (Disc 1).bin", Some(2_408_449)),
            ("Probe Game (Disc 2).bin", Some(1)),
            ("Probe Game (Disc 2).cue", Some(1)),
        ]);
        let link = current_link(FOLDER, &Found::Rom(Box::new(rom)), &exported);
        assert_eq!(link.state, LinkState::Differs);
        assert_eq!(
            link.detail.as_deref(),
            Some(
                "RomM lacks 2 of the 3 files written: Probe Game (Disc 2).bin, Probe Game \
                 (Disc 2).cue; RomM lists 1 file(s) the export did not write: Probe Game \
                 (Disc 1).cue; Probe Game (Disc 1).bin is 2408448 bytes in RomM and 2408449 as \
                 written"
            )
        );
        assert_eq!(some_of(&["a", "b", "c", "d", "e"]), "a, b, c and 2 more");
    }

    #[test]
    fn a_missing_rom_and_one_not_found_say_so() {
        let mut rom = probe_game();
        rom.missing_from_fs = true;
        let missing = current_link(FOLDER, &Found::Rom(Box::new(rom.clone())), &[]);
        assert_eq!(missing.state, LinkState::Missing);
        assert_eq!(missing.rom_id, Some(rom.id));

        let unseen = current_link(
            FOLDER,
            &Found::NotFound {
                looked_through_all: true,
            },
            &[],
        );
        assert_eq!(unseen.state, LinkState::Unseen);
        assert_eq!(unseen.rom_id, None);
        assert_eq!(
            unseen.detail.as_deref(),
            Some("RomM has no game in psx/Probe Game (USA) yet; a scan of its platform finds it")
        );
        let partly = current_link(
            FOLDER,
            &Found::NotFound {
                looked_through_all: false,
            },
            &[],
        );
        assert!(partly.detail.unwrap().contains("first 5000"));
        let no_platform = current_link(FOLDER, &Found::NoPlatform, &[]);
        assert_eq!(
            no_platform.detail.as_deref(),
            Some("RomM has no psx platform yet; a scan of its folder makes one")
        );
    }

    #[test]
    fn a_removed_export_is_followed_until_romm_marks_it_missing() {
        let mut rom = probe_game();
        let listed = removed_link(Some(&rom)).unwrap();
        assert_eq!(listed.state, LinkState::Linked);
        assert!(listed.detail.unwrap().contains("until a scan"));
        rom.missing_from_fs = true;
        assert_eq!(removed_link(Some(&rom)).unwrap().state, LinkState::Missing);
        assert_eq!(removed_link(None), None);
    }
}
