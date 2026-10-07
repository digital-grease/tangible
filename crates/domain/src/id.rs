// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Opaque public identifiers.
//!
//! Every externally visible identifier is a UUIDv7. v7 is time-ordered, which
//! keeps index locality without exposing a guessable sequence the way an
//! incrementing integer would.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Failure to parse an identifier from text.
#[derive(Debug, thiserror::Error)]
#[error("invalid {kind} identifier")]
pub struct IdParseError {
    kind: &'static str,
}

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generate a new time-ordered identifier.
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wrap an existing UUID, e.g. one loaded from the database.
            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// The underlying UUID, for persistence and logging.
            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value)
                    .map(Self)
                    .map_err(|_| IdParseError { kind: stringify!($name) })
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

// The catalog hierarchy, from abstract work down to one logical disc. Keeping
// these distinct is the point of the model: a single ISO must never stand in
// for the whole intellectual, publication, and physical hierarchy.
uuid_id!(
    /// Identifies an abstract work, product, or collection.
    TitleId
);
uuid_id!(
    /// Identifies a specific published or assembled release of a title.
    EditionId
);
uuid_id!(
    /// Identifies a group of physical discs belonging to one edition.
    DiscSetId
);
uuid_id!(
    /// Identifies one logical physical disc within a set.
    DiscId
);

uuid_id!(
    /// Identifies a stored artifact: one disc image, including all of its
    /// component files. A CUE and its BINs share a single `ArtifactId`.
    ArtifactId
);
uuid_id!(
    /// Identifies one file within an artifact.
    ComponentId
);
uuid_id!(
    /// Identifies one import operation from a configured source.
    ImportJobId
);
uuid_id!(
    /// Identifies a request to derive a new artifact from an existing one.
    DerivationJobId
);
uuid_id!(
    /// Identifies a requested burn of an artifact to physical media.
    BurnJobId
);
uuid_id!(
    /// Identifies a single execution attempt of a burn job. A job may have
    /// several attempts; only one may be active per drive.
    BurnAttemptId
);
uuid_id!(
    /// Identifies an enrolled burn-worker container.
    WorkerId
);
uuid_id!(
    /// Identifies one optical drive as reported by a worker. Drive identity is
    /// worker identity plus hardware information, never `/dev/srN` alone.
    DriveId
);
uuid_id!(
    /// Identifies a physical disc actually produced by a burn attempt.
    PhysicalCopyId
);
uuid_id!(
    /// Identifies a request to erase the rewritable disc in one drive.
    ErasureId
);
uuid_id!(
    /// Identifies a configured outbound integration.
    IntegrationId
);
uuid_id!(
    /// Identifies one audit or operational event record.
    EventId
);

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_version_7() {
        let id = ArtifactId::generate();
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn generated_ids_are_unique() {
        let a = ArtifactId::generate();
        let b = ArtifactId::generate();
        assert_ne!(a, b);
    }

    #[test]
    fn ids_are_time_ordered() {
        let first = ArtifactId::generate();
        let second = ArtifactId::generate();
        assert!(first < second, "v7 identifiers must sort by creation time");
    }

    #[test]
    fn display_and_parse_round_trip() {
        let id = ArtifactId::generate();
        let parsed: ArtifactId = id.to_string().parse().expect("round trip");
        assert_eq!(id, parsed);
    }

    #[test]
    fn parsing_rejects_non_uuid_text() {
        assert!("not-a-uuid".parse::<ArtifactId>().is_err());
    }

    #[test]
    fn serde_representation_is_a_bare_string() {
        let id = ArtifactId::generate();
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{id}\""));
    }
}
