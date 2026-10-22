// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ripping the disc in a drive.
//!
//! A rip is the other direction from a burn: the drive's worker reads a disc
//! someone owns and sends what it read, which becomes an import. Like an
//! erasure it belongs to one drive, and only that drive's worker takes it.
//!
//! Unlike a burn or an erase, reading changes nothing on the disc, so a rip
//! can be withdrawn at any point before its import starts, and a failed one
//! is simply asked for again.
//!
//! ```text
//! queued -> reading -> uploading -> registering -> complete
//! queued | reading | uploading -> canceled
//! queued | reading | uploading -> refused | failed
//! registering -> failed           (the import failed)
//! ```
//!
//! `queued` reaches `refused` and `failed` only through the worker, which
//! decides from what is in the drive, or when a restarted worker's rip is
//! closed.

use sha2::{Digest as _, Sha256};

use crate::dvd::{DvdFile, MAX_VOB_BYTES};
use crate::enums::{LossCharacter, RipOriginal, RipState};
use crate::id::{ArtifactId, RipJobId};
use crate::manifest::{DecryptionRecord, Extensions, Lineage, ToolRef};

/// The schema a decrypted copy's fingerprint is computed over.
pub const COPY_FINGERPRINT_SCHEMA: &str = "org.tangible.rip-decryption/v1";

/// A transition the machine does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a rip cannot go from {from} to {to}")]
pub struct RipTransitionError {
    /// Where it was.
    pub from: RipState,
    /// Where it was asked to go.
    pub to: RipState,
}

/// The files a CD rip produces: cdrdao's table of contents and its data.
///
/// The worker may send only these, or [`IMAGE_OUTPUT`]. Fixed names keep a
/// worker from choosing where in the staging area its bytes land, and say
/// what the import will find.
pub const CD_OUTPUTS: [&str; 2] = ["disc.toc", "disc.bin"];

/// The file a DVD or Blu-ray rip produces: the disc's sectors in order, an
/// image of its filesystem.
pub const IMAGE_OUTPUT: &str = "disc.iso";

/// Every name a rip may send.
pub const RIP_OUTPUTS: [&str; 3] = ["disc.toc", "disc.bin", IMAGE_OUTPUT];

/// The largest table of contents a rip may send: cdrdao's are a few
/// kilobytes.
pub const MAX_TOC_BYTES: u64 = 1024 * 1024;

/// The largest CD data file: a 99-minute CD of raw 2352-byte sectors is
/// about 1.05 GB; this leaves room and no more.
pub const MAX_CD_DATA_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The largest disc image: a quad-layer BDXL holds 128 GB.
pub const MAX_IMAGE_BYTES: u64 = 128 * 1000 * 1000 * 1000;

/// The folder a decrypted DVD's video files are sent in: `VIDEO_TS/` and
/// one of the names DVD-Video defines, nothing deeper.
pub const VIDEO_TS_DIR: &str = "VIDEO_TS/";

/// What a decrypted copy's files are sent under when it travels beside an
/// encrypted original, so the two are never mistaken for one image.
pub const DECRYPTED_PREFIX: &str = "decrypted/";

/// The largest DVD information file or backup a rip may send. Real ones are
/// tens or hundreds of kilobytes.
pub const MAX_IFO_BYTES: u64 = 16 * 1024 * 1024;

impl RipState {
    /// Whether the rip is still going.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Reading | Self::Uploading | Self::Registering
        )
    }

    /// Whether nothing further can happen to it.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !self.is_open()
    }

    /// Whether the rip occupies its drive: from when the worker takes it
    /// until everything it read has been sent.
    #[must_use]
    pub const fn holds_the_drive(self) -> bool {
        matches!(self, Self::Reading | Self::Uploading)
    }

    /// Whether a person may still withdraw it. Once the import has started,
    /// the rip ends with the import.
    #[must_use]
    pub const fn can_cancel(self) -> bool {
        matches!(self, Self::Queued | Self::Reading | Self::Uploading)
    }

    /// Whether the worker may send files now.
    #[must_use]
    pub const fn accepts_uploads(self) -> bool {
        self.holds_the_drive()
    }

    /// Move to `next`, if the machine allows it.
    ///
    /// # Errors
    ///
    /// [`RipTransitionError`] for any other move.
    pub fn transition_to(self, next: Self) -> Result<Self, RipTransitionError> {
        let allowed = match self {
            Self::Queued => matches!(
                next,
                Self::Reading | Self::Canceled | Self::Refused | Self::Failed
            ),
            Self::Reading => matches!(
                next,
                Self::Uploading | Self::Canceled | Self::Refused | Self::Failed
            ),
            Self::Uploading => matches!(
                next,
                Self::Registering | Self::Canceled | Self::Refused | Self::Failed
            ),
            Self::Registering => matches!(next, Self::Complete | Self::Failed),
            Self::Complete | Self::Canceled | Self::Refused | Self::Failed => false,
        };
        if allowed {
            Ok(next)
        } else {
            Err(RipTransitionError {
                from: self,
                to: next,
            })
        }
    }
}

/// Whether `name` is one of the files a CD rip may send.
#[must_use]
pub fn is_cd_output(name: &str) -> bool {
    CD_OUTPUTS.contains(&name)
}

/// The DVD-Video file a name sent as part of a `VIDEO_TS` tree is, if it is
/// one: `VIDEO_TS/` and a name DVD-Video defines, exactly as it spells it.
#[must_use]
pub fn video_ts_member(name: &str) -> Option<DvdFile> {
    name.strip_prefix(VIDEO_TS_DIR).and_then(DvdFile::classify)
}

/// Whether `name` is one of the files any rip may send: a CD's two files, a
/// disc image, a decrypted `VIDEO_TS` tree's files, or those under
/// [`DECRYPTED_PREFIX`].
#[must_use]
pub fn is_rip_output(name: &str) -> bool {
    RIP_OUTPUTS.contains(&name)
        || video_ts_member(name).is_some()
        || name
            .strip_prefix(DECRYPTED_PREFIX)
            .and_then(video_ts_member)
            .is_some()
}

/// The largest a file of this name may be, when it is one a rip may send.
#[must_use]
pub fn max_output_bytes(name: &str) -> Option<u64> {
    match name {
        "disc.toc" => return Some(MAX_TOC_BYTES),
        "disc.bin" => return Some(MAX_CD_DATA_BYTES),
        IMAGE_OUTPUT => return Some(MAX_IMAGE_BYTES),
        _ => {}
    }
    let member = name.strip_prefix(DECRYPTED_PREFIX).unwrap_or(name);
    match video_ts_member(member)? {
        DvdFile::VmgMenu | DvdFile::VtsMenu(_) | DvdFile::VtsTitle(..) => Some(MAX_VOB_BYTES),
        DvdFile::VmgIfo | DvdFile::VmgBup | DvdFile::VtsIfo(_) | DvdFile::VtsBup(_) => {
            Some(MAX_IFO_BYTES)
        }
    }
}

/// What a rip's original is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginalKind {
    /// A CD: `disc.toc` and `disc.bin`.
    Cd,
    /// A disc image: `disc.iso`, encrypted or not.
    Image,
    /// A decrypted DVD's `VIDEO_TS` tree, kept as the original.
    VideoTs,
}

/// The files of a whole rip, split into the original and, beside an
/// encrypted image, its decrypted copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSets {
    /// What the original is.
    pub original: OriginalKind,
    /// Its files, by the names they were sent under.
    pub original_files: Vec<String>,
    /// The decrypted copy's files, by the names they were sent under (with
    /// [`DECRYPTED_PREFIX`]). Empty when there is none.
    pub copy_files: Vec<String>,
}

/// Why a set of names is not a whole rip.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutputSetError {
    /// A name no rip sends.
    #[error("{0} is not a file a rip sends")]
    Unknown(String),
    /// Nothing, a part, or a mixture.
    #[error(
        "a rip is disc.toc and disc.bin, disc.iso, or a VIDEO_TS tree, never a mixture or a part"
    )]
    NotWhole,
    /// A `VIDEO_TS` tree without its video manager.
    #[error("a VIDEO_TS tree must include VIDEO_TS/VIDEO_TS.IFO")]
    NoVideoManager,
    /// A decrypted tree as the original of a rip that did not choose one.
    #[error("only a rip asked to keep a decrypted original may send a VIDEO_TS tree as one")]
    TreeNotChosen,
    /// A decrypted copy that the rip did not ask for, or without the
    /// encrypted image it belongs beside.
    #[error(
        "a decrypted copy is sent only beside disc.iso, by a rip asked to decrypt and keep the \
         encrypted original"
    )]
    CopyNotChosen,
}

/// Check that `names` are one whole rip, for a rip that asked to decrypt
/// (`decrypt`) and chose `keep`.
///
/// # Errors
///
/// [`OutputSetError`] for anything else.
pub fn check_output_set(
    names: &[String],
    decrypt: bool,
    keep: Option<RipOriginal>,
) -> Result<OutputSets, OutputSetError> {
    if let Some(name) = names.iter().find(|name| !is_rip_output(name)) {
        return Err(OutputSetError::Unknown(name.clone()));
    }
    let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
    names.sort_unstable();
    names.dedup();
    let (copy, original): (Vec<&str>, Vec<&str>) = names
        .into_iter()
        .partition(|name| name.starts_with(DECRYPTED_PREFIX));

    let has_manager = |files: &[&str], prefix: &str| {
        files
            .iter()
            .any(|name| name.strip_prefix(prefix) == Some("VIDEO_TS/VIDEO_TS.IFO"))
    };
    let kind = if original == ["disc.bin", "disc.toc"] {
        OriginalKind::Cd
    } else if original == [IMAGE_OUTPUT] {
        OriginalKind::Image
    } else if !original.is_empty() && original.iter().all(|name| video_ts_member(name).is_some()) {
        if !has_manager(&original, "") {
            return Err(OutputSetError::NoVideoManager);
        }
        if !(decrypt && keep == Some(RipOriginal::DecryptedOriginal)) {
            return Err(OutputSetError::TreeNotChosen);
        }
        OriginalKind::VideoTs
    } else {
        return Err(OutputSetError::NotWhole);
    };

    if !copy.is_empty() {
        if !(decrypt && keep == Some(RipOriginal::EncryptedOriginal) && kind == OriginalKind::Image)
        {
            return Err(OutputSetError::CopyNotChosen);
        }
        if !has_manager(&copy, DECRYPTED_PREFIX) {
            return Err(OutputSetError::NoVideoManager);
        }
    }
    Ok(OutputSets {
        original: kind,
        original_files: original.into_iter().map(str::to_owned).collect(),
        copy_files: copy.into_iter().map(str::to_owned).collect(),
    })
}

/// The lineage of a rip's decrypted copy, a derivative of the encrypted
/// original read in the same rip.
///
/// Its transformation is `decrypt_` and the system (`decrypt_css`), and
/// `read_from: disc` records that it was made from the disc in the drive,
/// not from the original's bytes: decryption only ever reads a disc. Such a
/// derivative is registered as the rip finishes, never queued as a
/// derivation job. Semantically equivalent: the same video, not the same
/// bytes.
#[must_use]
pub fn copy_lineage(parent: ArtifactId, rip: RipJobId, decryption: &DecryptionRecord) -> Lineage {
    let mut options = Extensions::new();
    options.insert("library".to_owned(), decryption.library.clone().into());
    options.insert(
        "library_version".to_owned(),
        decryption.library_version.clone().into(),
    );
    options.insert("read_from".to_owned(), "disc".into());
    options.insert("rip_id".to_owned(), rip.to_string().into());
    let transformation = format!("decrypt_{}", decryption.system);
    let document = serde_json::json!({
        "schema": COPY_FINGERPRINT_SCHEMA,
        "parent": parent.to_string(),
        "transformation": transformation,
        "options": options,
        "tool": { "name": decryption.tool, "version": decryption.tool_version },
    });
    // Sorted keys, as derivation fingerprints: serde_json's map is ordered.
    let fingerprint = hex::encode(Sha256::digest(
        serde_json::to_vec(&document).unwrap_or_default(),
    ));
    Lineage {
        parent_artifact_id: parent,
        transformation,
        tool: ToolRef {
            name: decryption.tool.clone(),
            version: decryption.tool_version.clone(),
        },
        normalized_options: options,
        command_fingerprint: fingerprint,
        loss_character: LossCharacter::SemanticallyEquivalent,
        started_at: None,
        completed_at: None,
    }
}

/// Why a rip that asks to decrypt may not go ahead.
///
/// Checked when it is asked for and again when its worker takes it: an
/// administrator may turn decryption off, or a worker be rebuilt without it,
/// in between, and a decrypting rip only ever runs on a worker that can and a
/// server that allows it (ADR 0013).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecryptionRefusal {
    /// The server's administrator has not turned decryption on.
    #[error("decryption is turned off on this server")]
    Off,
    /// The drive's worker did not report that it can decrypt.
    #[error("this drive's worker cannot decrypt; its image was not built to")]
    WorkerCannot,
}

impl DecryptionRefusal {
    /// The stable code, for a refusal to a person and a refused rip alike.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Off => "DECRYPTION_OFF",
            Self::WorkerCannot => "ENCRYPTED_NO_DECRYPTION",
        }
    }
}

/// Whether a rip may go ahead as far as decryption goes.
///
/// A rip that does not ask to decrypt always may: an encrypted disc is then
/// refused by the worker when it finds one, as before.
///
/// # Errors
///
/// [`DecryptionRefusal`] when it asks to decrypt and the server does not
/// allow it, or the worker cannot. The server's setting is checked first,
/// since it is the one an operator cannot change by rebuilding a worker.
pub const fn check_decryption(
    decrypt: bool,
    server_allows: bool,
    worker_can: bool,
) -> Result<(), DecryptionRefusal> {
    if !decrypt {
        Ok(())
    } else if !server_allows {
        Err(DecryptionRefusal::Off)
    } else if !worker_can {
        Err(DecryptionRefusal::WorkerCannot)
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn decryption_needs_the_server_and_the_worker() {
        for server in [false, true] {
            for worker in [false, true] {
                assert_eq!(check_decryption(false, server, worker), Ok(()));
            }
        }
        assert_eq!(check_decryption(true, true, true), Ok(()));
        assert_eq!(
            check_decryption(true, false, true),
            Err(DecryptionRefusal::Off)
        );
        assert_eq!(
            check_decryption(true, false, false),
            Err(DecryptionRefusal::Off)
        );
        assert_eq!(
            check_decryption(true, true, false),
            Err(DecryptionRefusal::WorkerCannot)
        );
        assert_eq!(DecryptionRefusal::Off.code(), "DECRYPTION_OFF");
        assert_eq!(
            DecryptionRefusal::WorkerCannot.code(),
            "ENCRYPTED_NO_DECRYPTION"
        );
    }
    use RipState::{Canceled, Complete, Failed, Queued, Reading, Refused, Registering, Uploading};

    #[test]
    fn a_rip_runs_from_queued_to_complete() {
        assert_eq!(Queued.transition_to(Reading), Ok(Reading));
        assert_eq!(Reading.transition_to(Uploading), Ok(Uploading));
        assert_eq!(Uploading.transition_to(Registering), Ok(Registering));
        assert_eq!(Registering.transition_to(Complete), Ok(Complete));
    }

    #[test]
    fn nothing_is_skipped() {
        assert!(Queued.transition_to(Uploading).is_err());
        assert!(
            Reading.transition_to(Registering).is_err(),
            "not without sending"
        );
        assert!(
            Uploading.transition_to(Complete).is_err(),
            "not without importing"
        );
    }

    #[test]
    fn reading_is_harmless_to_stop() {
        for state in [Queued, Reading, Uploading] {
            assert!(state.can_cancel());
            assert_eq!(state.transition_to(Canceled), Ok(Canceled));
        }
        assert!(!Registering.can_cancel());
        assert!(Registering.transition_to(Canceled).is_err());
    }

    #[test]
    fn the_worker_can_refuse_or_fail_until_it_has_sent_everything() {
        for state in [Queued, Reading, Uploading] {
            assert_eq!(state.transition_to(Refused), Ok(Refused));
            assert_eq!(state.transition_to(Failed), Ok(Failed));
        }
        assert!(Registering.transition_to(Refused).is_err());
        assert_eq!(
            Registering.transition_to(Failed),
            Ok(Failed),
            "the import failed"
        );
    }

    #[test]
    fn every_ending_is_final() {
        for end in [Complete, Canceled, Refused, Failed] {
            assert!(end.is_terminal());
            for next in RipState::all() {
                assert!(end.transition_to(*next).is_err(), "{end} -> {next}");
            }
        }
    }

    #[test]
    fn only_reading_and_sending_hold_the_drive() {
        for state in RipState::all() {
            assert_eq!(
                state.holds_the_drive(),
                matches!(state, Reading | Uploading),
                "{state}"
            );
            assert_eq!(state.accepts_uploads(), state.holds_the_drive());
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    const TREE: [&str; 4] = [
        "VIDEO_TS/VIDEO_TS.IFO",
        "VIDEO_TS/VIDEO_TS.BUP",
        "VIDEO_TS/VTS_01_0.IFO",
        "VIDEO_TS/VTS_01_1.VOB",
    ];

    fn copy() -> Vec<String> {
        TREE.iter()
            .map(|name| format!("{DECRYPTED_PREFIX}{name}"))
            .collect()
    }

    #[test]
    fn a_plain_rip_is_a_cd_pair_or_one_image() {
        let plain = |list: &[&str]| check_output_set(&names(list), false, None);
        assert_eq!(
            plain(&["disc.toc", "disc.bin"]).unwrap().original,
            OriginalKind::Cd
        );
        assert_eq!(
            plain(&["disc.bin", "disc.toc", "disc.toc"])
                .unwrap()
                .original,
            OriginalKind::Cd
        );
        let image = plain(&["disc.iso"]).unwrap();
        assert_eq!(image.original, OriginalKind::Image);
        assert!(image.copy_files.is_empty());
        for wrong in [
            &[][..],
            &["disc.toc"][..],
            &["disc.iso", "disc.toc"][..],
            &["disc.iso", "disc.toc", "disc.bin"][..],
            &["disc.iso", "VIDEO_TS/VIDEO_TS.IFO"][..],
        ] {
            assert_eq!(plain(wrong), Err(OutputSetError::NotWhole), "{wrong:?}");
        }
        assert_eq!(
            plain(&["other.iso"]),
            Err(OutputSetError::Unknown("other.iso".to_owned()))
        );
        // A tree, or a copy, needs a rip that chose one.
        assert_eq!(plain(&TREE), Err(OutputSetError::TreeNotChosen));
        let mut with_copy = names(&["disc.iso"]);
        with_copy.extend(copy());
        assert_eq!(
            check_output_set(&with_copy, false, None),
            Err(OutputSetError::CopyNotChosen)
        );
    }

    #[test]
    fn a_decrypting_rip_sends_what_was_chosen() {
        let encrypted = Some(RipOriginal::EncryptedOriginal);
        let decrypted = Some(RipOriginal::DecryptedOriginal);

        // Encrypted original: the image, with the copy beside it.
        let mut with_copy = names(&["disc.iso"]);
        with_copy.extend(copy());
        let sets = check_output_set(&with_copy, true, encrypted).unwrap();
        assert_eq!(sets.original, OriginalKind::Image);
        assert_eq!(sets.original_files, ["disc.iso"]);
        assert_eq!(sets.copy_files.len(), 4);
        // The disc turned out not to be encrypted: the image alone.
        assert!(check_output_set(&names(&["disc.iso"]), true, encrypted).is_ok());
        // A copy needs the image, and a tree as the original is the other choice.
        assert_eq!(
            check_output_set(&copy(), true, encrypted),
            Err(OutputSetError::NotWhole)
        );
        assert_eq!(
            check_output_set(&names(&TREE), true, encrypted),
            Err(OutputSetError::TreeNotChosen)
        );

        // Decrypted original: the tree.
        let sets = check_output_set(&names(&TREE), true, decrypted).unwrap();
        assert_eq!(sets.original, OriginalKind::VideoTs);
        assert!(sets.copy_files.is_empty());
        assert_eq!(
            check_output_set(&with_copy, true, decrypted),
            Err(OutputSetError::CopyNotChosen)
        );
        // Not encrypted after all: the image is the original.
        assert!(check_output_set(&names(&["disc.iso"]), true, decrypted).is_ok());

        // Trees need their video manager.
        assert_eq!(
            check_output_set(&names(&TREE[1..]), true, decrypted),
            Err(OutputSetError::NoVideoManager)
        );
        let mut headless = names(&["disc.iso"]);
        headless.extend(copy().into_iter().skip(1));
        assert_eq!(
            check_output_set(&headless, true, encrypted),
            Err(OutputSetError::NoVideoManager)
        );
    }

    #[test]
    fn a_decrypted_copy_says_what_it_was_made_with_and_from() {
        let parent = ArtifactId::generate();
        let rip = RipJobId::generate();
        let css = DecryptionRecord {
            system: "css".to_owned(),
            library: "libdvdcss".to_owned(),
            library_version: "1.6.0".to_owned(),
            tool: "dvdbackup".to_owned(),
            tool_version: "0.4.2-4.1".to_owned(),
        };
        let lineage = copy_lineage(parent, rip, &css);
        assert_eq!(lineage.parent_artifact_id, parent);
        assert_eq!(lineage.transformation, "decrypt_css");
        assert_eq!(lineage.tool.name, "dvdbackup");
        assert_eq!(lineage.normalized_options["read_from"], "disc");
        assert_eq!(lineage.normalized_options["rip_id"], rip.to_string());
        assert_eq!(lineage.normalized_options["library"], "libdvdcss");
        assert_eq!(
            lineage.loss_character,
            LossCharacter::SemanticallyEquivalent
        );
        assert_eq!(lineage.command_fingerprint.len(), 64);
        // Deterministic, and different for another rip of the same disc.
        assert_eq!(copy_lineage(parent, rip, &css), lineage);
        assert_ne!(
            copy_lineage(parent, RipJobId::generate(), &css).command_fingerprint,
            lineage.command_fingerprint
        );
        // Nothing about keys can be in it: it is built from versions alone.
        let text = serde_json::to_string(&lineage).unwrap();
        assert!(!text.to_ascii_lowercase().contains("key"), "{text}");
    }

    #[test]
    fn names_and_limits() {
        assert_eq!(max_output_bytes("disc.toc"), Some(MAX_TOC_BYTES));
        assert_eq!(max_output_bytes("disc.iso"), Some(MAX_IMAGE_BYTES));
        assert_eq!(max_output_bytes("disc.img"), None);
        assert!(is_rip_output("disc.iso") && !is_cd_output("disc.iso"));
        assert_eq!(
            max_output_bytes("VIDEO_TS/VTS_01_1.VOB"),
            Some(MAX_VOB_BYTES)
        );
        assert_eq!(
            max_output_bytes("decrypted/VIDEO_TS/VTS_01_0.IFO"),
            Some(MAX_IFO_BYTES)
        );
        for not in [
            "VIDEO_TS/notes.txt",
            "VIDEO_TS/sub/VTS_01_1.VOB",
            "video_ts/VIDEO_TS.IFO",
            "decrypted/disc.iso",
            "decrypted/decrypted/VIDEO_TS/VIDEO_TS.IFO",
            "VTS_01_1.VOB",
            "../VIDEO_TS/VIDEO_TS.IFO",
        ] {
            assert!(!is_rip_output(not), "{not}");
            assert_eq!(max_output_bytes(not), None, "{not}");
        }
    }

    #[test]
    fn a_cd_rip_sends_only_its_two_files() {
        assert!(is_cd_output("disc.toc"));
        assert!(is_cd_output("disc.bin"));
        for other in ["disc.iso", "../disc.bin", "DISC.BIN", "disc.toc/x", ""] {
            assert!(!is_cd_output(other), "{other}");
        }
    }
}
