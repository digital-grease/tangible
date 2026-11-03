// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Deleting an artifact, as a tombstone (ADR 0020).
//!
//! An artifact is kept, pending deletion for a grace period, or deleted: its
//! bytes gone, its record kept. A request may be undone until the grace
//! period ends; a tombstone is final. An original (imported, or dumped from
//! a disc) is deleted only when the request says so.

use time::OffsetDateTime;

use crate::enums::ArtifactOrigin;

/// Where an artifact is in being deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deletion {
    /// Kept.
    Kept,
    /// To be deleted once `delete_after` has passed, unless kept.
    Pending {
        /// When it was asked for.
        requested_at: OffsetDateTime,
        /// When the bytes may go.
        delete_after: OffsetDateTime,
    },
    /// Deleted: the bytes are gone, the record kept.
    Deleted {
        /// When.
        deleted_at: OffsetDateTime,
    },
}

/// Why a deletion step cannot be taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeletionError {
    /// An original, without the request saying so.
    #[error("this artifact is an original; deleting it needs confirm_original")]
    OriginalNotConfirmed,
    /// Already deleted: nothing can be done to a tombstone.
    #[error("this artifact is already deleted")]
    AlreadyDeleted,
    /// Not pending, so there is nothing to keep or to finish.
    #[error("this artifact is not pending deletion")]
    NotPending,
    /// Pending, but its grace period has not ended.
    #[error("this artifact's grace period has not ended")]
    NotDue,
}

/// Whether an artifact of this origin is an original, which deleting needs
/// saying so.
#[must_use]
pub fn is_original(origin: ArtifactOrigin) -> bool {
    matches!(
        origin,
        ArtifactOrigin::ImportedOriginal | ArtifactOrigin::PhysicalDump
    )
}

impl Deletion {
    /// Ask for deletion at `now`, the bytes to go after `grace`.
    ///
    /// Asking again while pending changes nothing: the first request's grace
    /// period stands.
    ///
    /// # Errors
    ///
    /// [`DeletionError::AlreadyDeleted`], or
    /// [`DeletionError::OriginalNotConfirmed`] for an original asked for
    /// without `confirm_original`.
    pub fn request(
        self,
        origin: ArtifactOrigin,
        confirm_original: bool,
        now: OffsetDateTime,
        grace: time::Duration,
    ) -> Result<Self, DeletionError> {
        match self {
            Self::Deleted { .. } => Err(DeletionError::AlreadyDeleted),
            Self::Pending { .. } => Ok(self),
            Self::Kept if is_original(origin) && !confirm_original => {
                Err(DeletionError::OriginalNotConfirmed)
            }
            Self::Kept => Ok(Self::Pending {
                requested_at: now,
                delete_after: now + grace,
            }),
        }
    }

    /// Undo a request.
    ///
    /// # Errors
    ///
    /// [`DeletionError::AlreadyDeleted`] once the bytes are gone, or
    /// [`DeletionError::NotPending`] for an artifact kept already.
    pub fn keep(self) -> Result<Self, DeletionError> {
        match self {
            Self::Pending { .. } => Ok(Self::Kept),
            Self::Deleted { .. } => Err(DeletionError::AlreadyDeleted),
            Self::Kept => Err(DeletionError::NotPending),
        }
    }

    /// Make the tombstone at `now`.
    ///
    /// # Errors
    ///
    /// [`DeletionError::NotDue`] before the grace period ends,
    /// [`DeletionError::NotPending`] for an artifact kept, or
    /// [`DeletionError::AlreadyDeleted`].
    pub fn complete(self, now: OffsetDateTime) -> Result<Self, DeletionError> {
        match self {
            Self::Pending { delete_after, .. } if delete_after <= now => {
                Ok(Self::Deleted { deleted_at: now })
            }
            Self::Pending { .. } => Err(DeletionError::NotDue),
            Self::Kept => Err(DeletionError::NotPending),
            Self::Deleted { .. } => Err(DeletionError::AlreadyDeleted),
        }
    }

    /// Whether new work may use the artifact's bytes: not while pending or
    /// deleted.
    #[must_use]
    pub fn usable(self) -> bool {
        self == Self::Kept
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const DAY: time::Duration = time::Duration::days(1);

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_793_620_800).expect("a time")
    }

    #[test]
    fn a_derivative_is_deleted_after_its_grace_period() {
        let pending = Deletion::Kept
            .request(ArtifactOrigin::Derived, false, now(), 7 * DAY)
            .unwrap();
        assert!(!pending.usable());
        assert_eq!(
            pending.complete(now() + 6 * DAY),
            Err(DeletionError::NotDue)
        );
        let deleted = pending.complete(now() + 7 * DAY).unwrap();
        assert_eq!(
            deleted,
            Deletion::Deleted {
                deleted_at: now() + 7 * DAY
            }
        );
        assert_eq!(deleted.keep(), Err(DeletionError::AlreadyDeleted));
        assert_eq!(
            deleted.request(ArtifactOrigin::Derived, true, now(), DAY),
            Err(DeletionError::AlreadyDeleted)
        );
    }

    #[test]
    fn an_original_needs_saying_so() {
        for origin in [
            ArtifactOrigin::ImportedOriginal,
            ArtifactOrigin::PhysicalDump,
        ] {
            assert_eq!(
                Deletion::Kept.request(origin, false, now(), DAY),
                Err(DeletionError::OriginalNotConfirmed),
                "{origin:?}"
            );
            assert!(Deletion::Kept.request(origin, true, now(), DAY).is_ok());
        }
    }

    #[test]
    fn a_request_is_undone_by_keeping_and_asking_again_changes_nothing() {
        let pending = Deletion::Kept
            .request(ArtifactOrigin::Derived, false, now(), 7 * DAY)
            .unwrap();
        let again = pending
            .request(ArtifactOrigin::Derived, false, now() + DAY, 7 * DAY)
            .unwrap();
        assert_eq!(again, pending, "the first grace period stands");
        assert_eq!(pending.keep(), Ok(Deletion::Kept));
        assert_eq!(Deletion::Kept.keep(), Err(DeletionError::NotPending));
        assert_eq!(
            Deletion::Kept.complete(now()),
            Err(DeletionError::NotPending)
        );
    }

    #[test]
    fn no_grace_period_is_due_at_once() {
        let pending = Deletion::Kept
            .request(ArtifactOrigin::Derived, false, now(), time::Duration::ZERO)
            .unwrap();
        assert!(pending.complete(now()).is_ok());
    }
}
