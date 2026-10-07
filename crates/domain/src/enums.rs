// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Closed sets from the domain model.
//!
//! Every one persists as its textual value, never an ordinal.
//! Ordinals turn a reordered enum into silent data corruption; text makes a
//! renamed variant a loud parse failure instead, and keeps the database
//! readable.
//!
//! Each type round-trips through [`FromStr`] and [`Display`], and serde uses
//! the same strings, so the wire format, the database, and log output all
//! agree.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Returned when text does not name a variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not a valid {kind}")]
pub struct EnumParseError {
    /// The type that rejected the value.
    pub kind: &'static str,
    /// The text supplied.
    pub value: String,
}

macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)?
        }
        $( default = $default:ident )?
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $(
                $(#[$vmeta])*
                // Every variant is documented with the value it persists as,
                // so rustdoc shows the database and wire representation
                // without each variant needing a hand-written line.
                #[doc = concat!("Persisted as `", $text, "`.")]
                $variant
            ),+
        }

        impl $name {
            /// The persisted textual value.
            #[must_use]
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $( Self::$variant => $text ),+
                }
            }

            /// Every variant, in declaration order.
            #[must_use]
            pub const fn all() -> &'static [Self] {
                &[ $( Self::$variant ),+ ]
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = EnumParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $( $text => Ok(Self::$variant), )+
                    other => Err(EnumParseError {
                        kind: stringify!($name),
                        value: other.to_owned(),
                    }),
                }
            }
        }

        $(
            impl Default for $name {
                fn default() -> Self {
                    Self::$default
                }
            }
        )?
    };
}

// --- catalog ----------------------------------------------------------------

string_enum! {
    /// What kind of work a title represents.
    ///
    /// Selects metadata fields and presentation only. It must never alter
    /// storage or burn semantics: the canonical object is a disc, and a disc
    /// behaves the same whatever is on it.
    TitleKind {
        Movie => "movie",
        Television => "television",
        Game => "game",
        Software => "software",
        OperatingSystem => "operating_system",
        Music => "music",
        DataArchive => "data_archive",
        Training => "training",
        Unknown => "unknown",
        Custom => "custom",
    }
    default = Unknown
}

string_enum! {
    /// How the discs in a set relate to one another.
    SetKind {
        SingleDisc => "single_disc",
        MultiDisc => "multi_disc",
        SeasonBox => "season_box",
        InstallationSet => "installation_set",
        FeatureAndSupplements => "feature_and_supplements",
        Compilation => "compilation",
        Unknown => "unknown",
    }
    default = Unknown
}

string_enum! {
    /// Physical media family of a disc.
    MediaFamily {
        Cd => "cd",
        Dvd => "dvd",
        Bluray => "bluray",
        UhdBluray => "uhd_bluray",
        GdRom => "gd_rom",
        ProprietaryOptical => "proprietary_optical",
        Unknown => "unknown",
    }
    default = Unknown
}

string_enum! {
    /// What reproduction is expected to achieve for a disc.
    ///
    /// A claim must carry evidence and a source, and defaults to `Unknown`.
    /// This is the model's refusal to promise that a burned disc will satisfy
    /// console or player authentication.
    CompatibilityClaim {
        Unknown => "unknown",
        DataReproductionExpected => "data_reproduction_expected",
        PlayerCompatibilityExpected => "player_compatibility_expected",
        EmulatorCompatibilityExpected => "emulator_compatibility_expected",
        OriginalHardwareCompatibilityExpected => "original_hardware_compatibility_expected",
        KnownNotReproducible => "known_not_reproducible",
    }
    default = Unknown
}

// --- artifacts --------------------------------------------------------------

string_enum! {
    /// Where an artifact's bytes came from.
    ArtifactOrigin {
        ImportedOriginal => "imported_original",
        Derived => "derived",
        PhysicalDump => "physical_dump",
        Generated => "generated",
        ExternalReference => "external_reference",
    }
}

string_enum! {
    /// The shape of an artifact's contents.
    ArtifactKind {
        SingleFileImage => "single_file_image",
        MultiFileImage => "multi_file_image",
        DirectoryTree => "directory_tree",
        ArchiveContainer => "archive_container",
        RawTrackSet => "raw_track_set",
        Unknown => "unknown",
    }
    default = Unknown
}

string_enum! {
    /// Disc-image container format.
    ///
    /// Never derived from the file extension alone. An extension may raise
    /// confidence; structure decides.
    ArtifactFormat {
        Iso => "iso",
        CueBin => "cue_bin",
        TocBin => "toc_bin",
        CcdImgSub => "ccd_img_sub",
        MdsMdf => "mds_mdf",
        Chd => "chd",
        BdmvDirectory => "bdmv_directory",
        VideoTsDirectory => "video_ts_directory",
        Raw => "raw",
        Unknown => "unknown",
    }
    default = Unknown
}

string_enum! {
    /// Outcome of structural validation.
    ValidationState {
        Pending => "pending",
        Valid => "valid",
        ValidWithWarnings => "valid_with_warnings",
        Invalid => "invalid",
        Unsupported => "unsupported",
        Quarantined => "quarantined",
    }
    default = Pending
}

impl ValidationState {
    /// Whether an artifact in this state may be used as burn input.
    ///
    /// Warnings do not block: plenty of real preservation dumps carry benign
    /// structural oddities, and refusing them would make the tool useless for
    /// its actual purpose. Anything unvalidated or known-bad does block.
    #[must_use]
    pub const fn permits_burning(&self) -> bool {
        matches!(self, Self::Valid | Self::ValidWithWarnings)
    }
}

string_enum! {
    /// Whether an artifact is isolated pending review.
    QuarantineState {
        None => "none",
        Pending => "pending",
        Confirmed => "confirmed",
        Cleared => "cleared",
    }
    default = None
}

string_enum! {
    /// What role a file plays inside an artifact.
    ComponentRole {
        PrimaryImage => "primary_image",
        Descriptor => "descriptor",
        TrackData => "track_data",
        Subchannel => "subchannel",
        Metadata => "metadata",
        CertificateTree => "certificate_tree",
        FilesystemFile => "filesystem_file",
        Archive => "archive",
        Unknown => "unknown",
    }
    default = Unknown
}

string_enum! {
    /// Hash algorithms recorded against artifacts and components.
    HashAlgorithm {
        Sha256 => "sha256",
        Sha1 => "sha1",
        Md5 => "md5",
        Crc32 => "crc32",
        Blake3 => "blake3",
        ProviderSpecific => "provider_specific",
    }
}

impl HashAlgorithm {
    /// Whether this algorithm may establish internal artifact identity.
    ///
    /// Only SHA-256 may. The others exist to match preservation databases such
    /// as Redump, which publish CRC32, MD5 and SHA-1, useful for
    /// identification, not trustworthy for identity.
    #[must_use]
    pub const fn is_identity_bearing(&self) -> bool {
        matches!(self, Self::Sha256)
    }
}

string_enum! {
    /// How an artifact relates to a logical disc.
    DiscRelationship {
        RepresentationOf => "representation_of",
        Contains => "contains",
        SupplementFor => "supplement_for",
        Unknown => "unknown",
    }
    default = Unknown
}

string_enum! {
    /// How much fidelity a derivation preserves.
    LossCharacter {
        BitExactRepack => "bit_exact_repack",
        StructurallyEquivalent => "structurally_equivalent",
        SemanticallyEquivalent => "semantically_equivalent",
        Lossy => "lossy",
        Unknown => "unknown",
    }
    default = Unknown
}

impl LossCharacter {
    /// Whether the original bytes can be reconstructed from the derivative.
    ///
    /// Governs whether a derivative could ever substitute for its parent.
    /// `Unknown` is treated as "no": an unproven claim is not a guarantee.
    #[must_use]
    pub const fn is_reversible(&self) -> bool {
        matches!(self, Self::BitExactRepack)
    }
}

// --- policy -----------------------------------------------------------------

string_enum! {
    /// One step in a verification policy.
    ///
    /// A policy is an ordered list of these. `None` requires an explicit user
    /// choice and a warning, and it is not offered in the main burn flow at
    /// all.
    VerificationStep {
        None => "none",
        ToolVerify => "tool_verify",
        FilesystemCompare => "filesystem_compare",
        FullSectorReadback => "full_sector_readback",
        TrackHashCompare => "track_hash_compare",
        ProviderMatch => "provider_match",
    }
}

string_enum! {
    /// What to do with the medium once an attempt ends.
    ///
    /// A closed set rather than a boolean, because the interesting choice is
    /// the middle one: eject a disc that verified, and leave a disc that did
    /// not where an operator will find it rather than handing it back as
    /// though it were good.
    EjectPolicy {
        Never => "never",
        EjectOnSuccess => "eject_on_success",
        Always => "always",
    }
    default = EjectOnSuccess
}

impl EjectPolicy {
    /// Whether an attempt ending in `success` should eject.
    #[must_use]
    pub const fn ejects_after(&self, success: bool) -> bool {
        match self {
            Self::Never => false,
            Self::EjectOnSuccess => success,
            Self::Always => true,
        }
    }
}

impl VerificationStep {
    /// Whether this step compares written media against the source bytes.
    ///
    /// A tool reporting success is not verification: only reading the disc
    /// back and comparing establishes that the medium holds what was intended.
    #[must_use]
    pub const fn reads_media_back(&self) -> bool {
        matches!(
            self,
            Self::FilesystemCompare | Self::FullSectorReadback | Self::TrackHashCompare
        )
    }
}

// --- people -----------------------------------------------------------------

string_enum! {
    /// What a signed-in person may do, in three steps.
    ///
    /// A closed set and an ordering: each role can do everything the one
    /// below it can. The permissions each one carries are in
    /// [`crate::auth::Permission`], decided here and nowhere else.
    Role {
        /// Reads the library, the catalog, burns and discs. Changes nothing.
        Viewer => "viewer",
        /// Also imports, catalogues and burns: the day-to-day work.
        Operator => "operator",
        /// Also manages workers and people.
        Administrator => "administrator",
    }
    default = Viewer
}

// --- erasing ----------------------------------------------------------------

string_enum! {
    /// How thoroughly to erase a rewritable disc.
    ErasureMode {
        /// Enough to make the disc writable again. Quick on a CD-RW, but some
        /// drives erase a whole DVD-RW either way: the first one measured
        /// took 29 minutes.
        Quick => "quick",
        /// Writes over the whole disc. Up to an hour on a DVD-RW.
        Full => "full",
    }
    default = Quick
}

string_enum! {
    /// Where a request to erase a disc has got to. Transitions are in
    /// [`crate::erasure`].
    ErasureState {
        /// Asked for, waiting for the drive's worker.
        Queued => "queued",
        /// The worker has it and the drive is erasing.
        Erasing => "erasing",
        /// The disc was erased. Terminal.
        Erased => "erased",
        /// The disc was already blank, so nothing was done. Terminal.
        AlreadyBlank => "already_blank",
        /// The worker would not erase what was in the drive: no disc, or one
        /// that cannot be erased. Nothing was done. Terminal.
        Refused => "refused",
        /// The erase was attempted and did not succeed, or was interrupted.
        /// The disc may be partly erased. Terminal.
        Failed => "failed",
        /// Withdrawn before the worker took it. Terminal.
        Canceled => "canceled",
    }
    default = Queued
}

// --- workers and drives -----------------------------------------------------

string_enum! {
    /// Lifecycle state of an enrolled burn worker.
    WorkerStatus {
        Pending => "pending",
        Online => "online",
        Offline => "offline",
        Draining => "draining",
        Revoked => "revoked",
        Incompatible => "incompatible",
    }
    default = Pending
}

impl WorkerStatus {
    /// Whether the server may lease new work to this worker.
    ///
    /// Draining workers finish what they hold and take nothing new, which is
    /// what makes a redeploy safe while a burn is running.
    #[must_use]
    pub const fn accepts_new_work(&self) -> bool {
        matches!(self, Self::Online)
    }
}

string_enum! {
    /// Observed state of an optical drive.
    DriveStatus {
        Unknown => "unknown",
        ReadyEmpty => "ready_empty",
        ReadyWithMedia => "ready_with_media",
        Busy => "busy",
        TrayOpen => "tray_open",
        Missing => "missing",
        Error => "error",
        Disabled => "disabled",
    }
    default = Unknown
}

impl DriveStatus {
    /// Whether a burn could plausibly start on this drive right now.
    #[must_use]
    pub const fn can_begin_burn(&self) -> bool {
        matches!(self, Self::ReadyWithMedia)
    }
}

// --- integrations -----------------------------------------------------------

string_enum! {
    /// Kind of configured outbound integration.
    IntegrationKind {
        Romm => "romm",
        Qbittorrent => "qbittorrent",
        Sabnzbd => "sabnzbd",
        Nzbget => "nzbget",
        MetadataProvider => "metadata_provider",
        Webhook => "webhook",
        S3 => "s3",
        PeerInstance => "peer_instance",
    }
}

string_enum! {
    /// Condition of a produced physical disc.
    PhysicalCopyStatus {
        ProducedUnverified => "produced_unverified",
        Verified => "verified",
        VerificationFailed => "verification_failed",
        Degraded => "degraded",
        Lost => "lost",
        Destroyed => "destroyed",
        Unknown => "unknown",
    }
    default = Unknown
}

impl PhysicalCopyStatus {
    /// Whether this disc's story has ended.
    ///
    /// Only destruction. A disc that is lost may be found; one that failed
    /// verification may be checked again and found readable, or worse. A disc
    /// that has been destroyed is gone, and the record stays only so the
    /// history of what was burned remains true.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Destroyed)
    }

    /// Whether this disc should be taken out of circulation.
    ///
    /// The question an operator is really asking when they look at a shelf:
    /// can I use this, or should it go in the bin? Anything not known to hold
    /// what was intended answers the same way, because a disc that might be
    /// wrong is worse than no disc: it gets filed, found later, and trusted.
    #[must_use]
    pub const fn should_be_destroyed(&self) -> bool {
        matches!(self, Self::VerificationFailed | Self::Degraded)
    }

    /// Whether an operator may record this as the disc's new condition.
    ///
    /// The record of a burn is not editable, but the condition of the disc it
    /// produced is: discs rot, get scratched, get lost, and get thrown away,
    /// and none of that changes what was written.
    ///
    /// Two things are refused. A destroyed disc accepts nothing further,
    /// because it no longer exists to have a condition. And nothing may be
    /// promoted to `verified` by hand: that word means a disc was read back
    /// and matched, and only a check can establish it.
    #[must_use]
    pub fn accepts_condition(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        !matches!(next, Self::Verified | Self::VerificationFailed)
    }

    /// The condition a completed check establishes.
    ///
    /// The one path to `verified`, and the reason it is a separate method: a
    /// check is evidence, and evidence is what that word is supposed to mean.
    #[must_use]
    pub const fn after_check(matched: bool) -> Self {
        if matched {
            Self::Verified
        } else {
            Self::VerificationFailed
        }
    }
}

// --- derivations ------------------------------------------------------------

string_enum! {
    /// A transformation Tangible knows how to run.
    Transformation {
        /// A CD image (CUE/BIN, TOC/BIN, or a CD-sized ISO) to a CD CHD,
        /// keeping its tracks.
        ChdCreateCd => "chd_create_cd",
        /// A DVD or Blu-ray ISO to a DVD CHD.
        ChdCreateDvd => "chd_create_dvd",
    }
}

string_enum! {
    /// Where a derivation job has got to.
    DerivationJobState {
        /// Accepted, not started.
        Queued => "queued",
        /// Being worked.
        Running => "running",
        /// The derivative exists. Terminal.
        Complete => "complete",
        /// Failed for a reason that may pass on another try; held off, then
        /// offered again.
        FailedRetryable => "failed_retryable",
        /// Failed for a reason that will not change. Terminal.
        FailedTerminal => "failed_terminal",
        /// Withdrawn before it ran. Terminal.
        Canceled => "canceled",
    }
    default = Queued
}

string_enum! {
    /// A compression codec chdman can use inside a CHD.
    ChdCodec {
        /// LZMA, for CD data frames.
        Cdlz => "cdlz",
        /// Deflate, for CD data frames.
        Cdzl => "cdzl",
        /// FLAC, for CD audio frames.
        Cdfl => "cdfl",
        /// LZMA.
        Lzma => "lzma",
        /// Deflate.
        Zlib => "zlib",
        /// Huffman.
        Huff => "huff",
        /// FLAC.
        Flac => "flac",
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Round-tripping every variant of every enum through text catches a
    /// mismatch between `as_str` and `FromStr` that a spot check would miss.
    macro_rules! assert_round_trips {
        ($($ty:ty),+ $(,)?) => {
            $(
                for variant in <$ty>::all() {
                    let text = variant.as_str();
                    let parsed: $ty = text.parse().unwrap_or_else(|error| {
                        panic!("{} failed to parse its own {text:?}: {error}", stringify!($ty))
                    });
                    assert_eq!(&parsed, variant, "{} round trip", stringify!($ty));

                    let json = serde_json::to_string(variant).expect("serialize");
                    assert_eq!(
                        json,
                        format!("\"{text}\""),
                        "{} serde must match as_str", stringify!($ty)
                    );
                    let from_json: $ty = serde_json::from_str(&json).expect("deserialize");
                    assert_eq!(&from_json, variant, "{} serde round trip", stringify!($ty));
                }
            )+
        };
    }

    #[test]
    fn every_enum_round_trips_through_text_and_serde() {
        assert_round_trips!(
            TitleKind,
            SetKind,
            MediaFamily,
            CompatibilityClaim,
            ArtifactOrigin,
            ArtifactKind,
            ArtifactFormat,
            ValidationState,
            QuarantineState,
            ComponentRole,
            HashAlgorithm,
            DiscRelationship,
            LossCharacter,
            VerificationStep,
            EjectPolicy,
            WorkerStatus,
            DriveStatus,
            IntegrationKind,
            PhysicalCopyStatus,
        );
    }

    #[test]
    fn only_destruction_ends_a_discs_story() {
        // A lost disc may be found; a degraded one may be checked again.
        assert!(PhysicalCopyStatus::Destroyed.is_terminal());
        for open in [
            PhysicalCopyStatus::Lost,
            PhysicalCopyStatus::Degraded,
            PhysicalCopyStatus::VerificationFailed,
            PhysicalCopyStatus::ProducedUnverified,
            PhysicalCopyStatus::Verified,
        ] {
            assert!(!open.is_terminal(), "{open}");
        }
    }

    #[test]
    fn a_disc_that_might_be_wrong_is_flagged_for_destruction() {
        // Worse than no disc: it gets filed, found later, and trusted.
        assert!(PhysicalCopyStatus::VerificationFailed.should_be_destroyed());
        assert!(PhysicalCopyStatus::Degraded.should_be_destroyed());
        assert!(!PhysicalCopyStatus::Verified.should_be_destroyed());
        // Unverified is not the same as wrong. Nothing read it, so nothing is
        // known, and destroying it would be a decision the system made for an
        // operator who might simply want to check it.
        assert!(!PhysicalCopyStatus::ProducedUnverified.should_be_destroyed());
    }

    #[test]
    fn nothing_becomes_verified_without_a_check() {
        // The word means a disc was read back and matched. Letting an
        // operator type it would make the strongest claim in the system the
        // cheapest one to make.
        for from in PhysicalCopyStatus::all() {
            assert!(
                !from.accepts_condition(PhysicalCopyStatus::Verified),
                "{from} must not be promoted to verified by hand"
            );
            assert!(!from.accepts_condition(PhysicalCopyStatus::VerificationFailed));
        }
        assert_eq!(
            PhysicalCopyStatus::after_check(true),
            PhysicalCopyStatus::Verified
        );
        assert_eq!(
            PhysicalCopyStatus::after_check(false),
            PhysicalCopyStatus::VerificationFailed
        );
    }

    #[test]
    fn a_destroyed_disc_accepts_no_further_condition() {
        for next in PhysicalCopyStatus::all() {
            assert!(
                !PhysicalCopyStatus::Destroyed.accepts_condition(*next),
                "{next}"
            );
        }
    }

    #[test]
    fn an_operator_may_record_what_happened_to_a_disc() {
        // Discs rot, get scratched, get lost and get thrown away, and none of
        // that changes what was written to them.
        for next in [
            PhysicalCopyStatus::Degraded,
            PhysicalCopyStatus::Lost,
            PhysicalCopyStatus::Destroyed,
            PhysicalCopyStatus::Unknown,
        ] {
            assert!(
                PhysicalCopyStatus::Verified.accepts_condition(next),
                "{next}"
            );
        }
    }

    #[test]
    fn a_disc_that_failed_verification_is_not_handed_back() {
        // The middle policy is the reason this is an enum and not a boolean:
        // a disc that verified is ejected, and one that did not stays in the
        // drive where an operator will find it.
        assert!(EjectPolicy::EjectOnSuccess.ejects_after(true));
        assert!(!EjectPolicy::EjectOnSuccess.ejects_after(false));
        assert!(!EjectPolicy::Never.ejects_after(true));
        assert!(EjectPolicy::Always.ejects_after(false));
    }

    #[test]
    fn unknown_text_is_rejected_rather_than_defaulted() {
        // Silently mapping an unrecognized value onto a default would turn a
        // schema mismatch into wrong data.
        let error = "not_a_format"
            .parse::<ArtifactFormat>()
            .expect_err("reject");
        assert_eq!(error.kind, "ArtifactFormat");
        assert_eq!(error.value, "not_a_format");
    }

    #[test]
    fn defaults_are_the_conservative_variant() {
        assert_eq!(TitleKind::default(), TitleKind::Unknown);
        assert_eq!(ArtifactFormat::default(), ArtifactFormat::Unknown);
        assert_eq!(ValidationState::default(), ValidationState::Pending);
        assert_eq!(QuarantineState::default(), QuarantineState::None);
        // The claim that promises the least.
        assert_eq!(CompatibilityClaim::default(), CompatibilityClaim::Unknown);
    }

    #[test]
    fn only_sha256_bears_identity() {
        // Only SHA-256 establishes internal identity; the weaker digests exist
        // to match external preservation databases, not to prove identity.
        assert!(HashAlgorithm::Sha256.is_identity_bearing());
        for weaker in [
            HashAlgorithm::Sha1,
            HashAlgorithm::Md5,
            HashAlgorithm::Crc32,
            HashAlgorithm::Blake3,
            HashAlgorithm::ProviderSpecific,
        ] {
            assert!(
                !weaker.is_identity_bearing(),
                "{weaker} must not establish internal identity"
            );
        }
    }

    #[test]
    fn only_validated_artifacts_may_be_burned() {
        assert!(ValidationState::Valid.permits_burning());
        // Benign structural oddities are common in real preservation dumps.
        assert!(ValidationState::ValidWithWarnings.permits_burning());
        for blocked in [
            ValidationState::Pending,
            ValidationState::Invalid,
            ValidationState::Unsupported,
            ValidationState::Quarantined,
        ] {
            assert!(!blocked.permits_burning(), "{blocked} must block burning");
        }
    }

    #[test]
    fn tool_verify_alone_is_not_read_back_verification() {
        // A successful tool exit is not verified media.
        assert!(!VerificationStep::ToolVerify.reads_media_back());
        assert!(!VerificationStep::None.reads_media_back());
        assert!(VerificationStep::FullSectorReadback.reads_media_back());
        assert!(VerificationStep::TrackHashCompare.reads_media_back());
    }

    #[test]
    fn only_bit_exact_derivations_are_reversible() {
        assert!(LossCharacter::BitExactRepack.is_reversible());
        // An unproven claim is not a guarantee.
        assert!(!LossCharacter::Unknown.is_reversible());
        assert!(!LossCharacter::Lossy.is_reversible());
        assert!(!LossCharacter::SemanticallyEquivalent.is_reversible());
    }

    #[test]
    fn only_online_workers_receive_new_leases() {
        assert!(WorkerStatus::Online.accepts_new_work());
        for refused in [
            WorkerStatus::Pending,
            WorkerStatus::Offline,
            // Draining finishes current work but takes none.
            WorkerStatus::Draining,
            WorkerStatus::Revoked,
            WorkerStatus::Incompatible,
        ] {
            assert!(
                !refused.accepts_new_work(),
                "{refused} must not be leased to"
            );
        }
    }

    #[test]
    fn a_burn_needs_media_actually_present() {
        assert!(DriveStatus::ReadyWithMedia.can_begin_burn());
        assert!(!DriveStatus::ReadyEmpty.can_begin_burn());
        assert!(!DriveStatus::TrayOpen.can_begin_burn());
        assert!(!DriveStatus::Busy.can_begin_burn());
    }
}
