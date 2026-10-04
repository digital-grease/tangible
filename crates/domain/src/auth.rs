// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Who may do what, and what a valid account looks like.
//!
//! The rules, with no I/O: hashing a password, keeping a session and checking
//! a request belong to the API crate. Keeping the decisions here means the one
//! table that says what each role may do is a pure function, tested without a
//! server, and every route reads it rather than carrying its own idea.

use crate::enums::Role;

/// Something a request may need permission to do.
///
/// Named after what it does rather than the route that does it, so a new
/// route asks which existing permission it falls under before anyone writes a
/// new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Read the library, catalog, burns, discs and imports.
    Read,
    /// Import files into the library.
    Import,
    /// Create and edit catalog records: titles, editions, sets, discs, links.
    Catalog,
    /// Queue, cancel, retry and account for burns, and record checks on discs.
    Burn,
    /// Erase a rewritable disc in a drive, destroying what is on it.
    ///
    /// Its own permission rather than part of [`Self::Burn`]: a burn consumes
    /// a blank disc, while an erase destroys a disc that may hold somebody's
    /// only copy of something.
    Erase,
    /// Issue worker enrollment tokens and manage workers.
    ManageWorkers,
    /// Create, list and change accounts.
    ManageUsers,
}

impl Permission {
    /// The permission's name, for logs and refusals.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Import => "import",
            Self::Catalog => "catalog",
            Self::Burn => "burn",
            Self::Erase => "erase",
            Self::ManageWorkers => "manage_workers",
            Self::ManageUsers => "manage_users",
        }
    }

    /// The least role that carries this permission.
    #[must_use]
    pub const fn least_role(self) -> Role {
        match self {
            Self::Read => Role::Viewer,
            Self::Import | Self::Catalog | Self::Burn => Role::Operator,
            Self::Erase | Self::ManageWorkers | Self::ManageUsers => Role::Administrator,
        }
    }
}

impl Role {
    /// Whether this role carries a permission.
    ///
    /// Roles are ordered, so a permission held by a role is held by every role
    /// above it. That is what keeps the table small enough to read.
    #[must_use]
    pub fn allows(self, permission: Permission) -> bool {
        self >= permission.least_role()
    }
}

/// Shortest password accepted, in characters.
///
/// Long rather than clever: length is what makes a password hard to guess,
/// and composition rules mostly make it hard to remember.
pub const MIN_PASSWORD_CHARS: usize = 12;

/// Longest password accepted, in bytes. Argon2 takes anything, but a
/// megabyte of password is a way to make the server hash a megabyte.
pub const MAX_PASSWORD_BYTES: usize = 256;

/// Why an account could not be created as asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    /// A username outside the allowed shape.
    #[error("a username is 1 to 64 lowercase letters, digits, '.', '_' or '-'")]
    InvalidUsername,
    /// A password too short to be worth having.
    #[error("a password needs at least {MIN_PASSWORD_CHARS} characters")]
    PasswordTooShort,
    /// A password long enough to be an attack on the hasher.
    #[error("a password can be at most {MAX_PASSWORD_BYTES} bytes")]
    PasswordTooLong,
}

/// Check a username and return it as stored.
///
/// Lowercase letters, digits, `.`, `_` and `-`, 1 to 64 of them. Stored
/// lowercased, so `Alice` and `alice` are one account rather than two that
/// look alike in a list.
///
/// # Errors
///
/// [`AccountError::InvalidUsername`] for anything else.
pub fn normalize_username(username: &str) -> Result<String, AccountError> {
    let username = username.trim().to_ascii_lowercase();
    let valid = (1..=64).contains(&username.len())
        && username.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        });
    if valid {
        Ok(username)
    } else {
        Err(AccountError::InvalidUsername)
    }
}

/// Check a new password's length.
///
/// # Errors
///
/// [`AccountError::PasswordTooShort`] or [`AccountError::PasswordTooLong`].
pub fn check_new_password(password: &str) -> Result<(), AccountError> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(AccountError::PasswordTooShort);
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(AccountError::PasswordTooLong);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_viewer_reads_and_changes_nothing() {
        assert!(Role::Viewer.allows(Permission::Read));
        for permission in [
            Permission::Import,
            Permission::Catalog,
            Permission::Burn,
            Permission::ManageWorkers,
            Permission::ManageUsers,
        ] {
            assert!(!Role::Viewer.allows(permission), "{permission:?}");
        }
    }

    #[test]
    fn an_operator_does_the_work_but_not_the_administration() {
        for permission in [
            Permission::Read,
            Permission::Import,
            Permission::Catalog,
            Permission::Burn,
        ] {
            assert!(Role::Operator.allows(permission), "{permission:?}");
        }
        assert!(!Role::Operator.allows(Permission::ManageWorkers));
        assert!(!Role::Operator.allows(Permission::ManageUsers));
    }

    #[test]
    fn an_administrator_may_do_everything() {
        for permission in [
            Permission::Read,
            Permission::Import,
            Permission::Catalog,
            Permission::Burn,
            Permission::ManageWorkers,
            Permission::ManageUsers,
        ] {
            assert!(Role::Administrator.allows(permission), "{permission:?}");
        }
    }

    #[test]
    fn a_role_is_stored_as_its_name() {
        assert_eq!(Role::Administrator.as_str(), "administrator");
        assert_eq!("operator".parse::<Role>(), Ok(Role::Operator));
        assert!("root".parse::<Role>().is_err());
    }

    #[test]
    fn a_username_is_stored_lowercase_and_plain() {
        assert_eq!(normalize_username(" Alice "), Ok("alice".to_owned()));
        assert_eq!(
            normalize_username("m.flowers-2"),
            Ok("m.flowers-2".to_owned())
        );
        for bad in ["", "a b", "émile", "../x", "a@b", &"x".repeat(65)] {
            assert_eq!(
                normalize_username(bad),
                Err(AccountError::InvalidUsername),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_password_has_to_be_long_and_not_absurdly_so() {
        assert_eq!(
            check_new_password("short"),
            Err(AccountError::PasswordTooShort)
        );
        assert_eq!(check_new_password("twelve chars"), Ok(()));
        // Counted in characters, so a passphrase in another script is not
        // penalised for being multi-byte.
        assert_eq!(check_new_password("пароль-пароль"), Ok(()));
        assert_eq!(
            check_new_password(&"x".repeat(MAX_PASSWORD_BYTES + 1)),
            Err(AccountError::PasswordTooLong)
        );
    }
}
