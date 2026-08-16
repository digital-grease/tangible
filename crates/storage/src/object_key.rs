// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Object keys and their on-disk layout.

use std::path::PathBuf;

use tangible_domain::Sha256Digest;

/// Directory holding all content-addressed objects.
pub const OBJECTS_DIR: &str = "objects";

/// Subdirectory naming the digest algorithm, so a future algorithm can be
/// added alongside rather than migrating everything.
pub const ALGORITHM_DIR: &str = "sha256";

/// Where partially written objects live before they are named.
///
/// Inside the objects tree on purpose: `rename` is only atomic within one
/// filesystem, so the temporary file must not sit on a different mount.
pub const INCOMING_DIR: &str = "incoming";

/// The location of one object within a library root.
///
/// Two levels of two hex characters fan the tree out to 65,536 directories
/// before any of them fills, which keeps directory listings usable and avoids
/// the pathological single-directory case on filesystems that degrade with
/// very large directories.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey {
    digest: Sha256Digest,
}

impl ObjectKey {
    /// Key an object by its content digest.
    #[must_use]
    pub const fn new(digest: Sha256Digest) -> Self {
        Self { digest }
    }

    /// The digest this key addresses.
    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    /// Path relative to the library root: `objects/sha256/ab/cd/<64 hex>`.
    ///
    /// Canonical objects carry no extension. The original filename lives in
    /// the manifest and the database, because the same bytes may have arrived
    /// under many names and the store deduplicates them into one object.
    #[must_use]
    pub fn relative_path(&self) -> PathBuf {
        let hex = self.digest.to_hex();
        // A SHA-256 hex string is always 64 characters, so these slices are
        // always in range.
        let (first, rest) = hex.split_at(2);
        let (second, _) = rest.split_at(2);
        PathBuf::from(OBJECTS_DIR)
            .join(ALGORITHM_DIR)
            .join(first)
            .join(second)
            .join(&hex)
    }
}

impl From<Sha256Digest> for ObjectKey {
    fn from(digest: Sha256Digest) -> Self {
        Self::new(digest)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn key(hex: &str) -> ObjectKey {
        ObjectKey::new(hex.parse::<Sha256Digest>().expect("valid digest"))
    }

    #[test]
    fn layout_matches_the_documented_shape() {
        // The documented shape: sha256/<first-2>/<next-2>/<full-lowercase-digest>
        assert_eq!(
            key(EMPTY).relative_path(),
            PathBuf::from("objects/sha256/e3/b0").join(EMPTY)
        );
    }

    #[test]
    fn the_file_name_is_the_whole_digest_not_the_remainder() {
        // Storing only the tail would make the file name ambiguous once the
        // prefix directories are stripped, e.g. in a backup listing.
        let path = key(EMPTY).relative_path();
        let name = path.file_name().expect("has a file name");
        assert_eq!(name.to_string_lossy(), EMPTY);
    }

    #[test]
    fn canonical_objects_have_no_extension() {
        assert_eq!(key(EMPTY).relative_path().extension(), None);
    }

    #[test]
    fn distinct_digests_fan_out_across_directories() {
        let a = key(EMPTY);
        let b = key(&"f".repeat(64));
        assert_ne!(a.relative_path().parent(), b.relative_path().parent());
    }

    #[test]
    fn the_path_is_relative_so_it_cannot_escape_a_root() {
        let path = key(EMPTY).relative_path();
        assert!(path.is_relative());
        assert!(!path.to_string_lossy().contains(".."));
    }
}
