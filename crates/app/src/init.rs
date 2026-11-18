// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `tangible init`: the secrets a new deployment needs, made once.
//!
//! compose.yaml's `init` service runs it before anything else
//! (docs/plans/install.md, I2), so that nobody has to invent a password
//! before the first `docker compose up`. It keeps two files in the secrets
//! volume:
//!
//! - `postgres-password`, which PostgreSQL is created with and the server,
//!   its migrations and the backup connect with. PostgreSQL's image reads it
//!   after dropping to its own user, so it is readable by anyone in the
//!   containers that mount the volume, which are those four.
//! - `secret-key`, which integration secrets are sealed with, readable by
//!   the server's user alone.
//!
//! Each is made only when absent, and never printed. An install from before
//! this keeps its `POSTGRES_PASSWORD` in `.env`, which its database was
//! created with; given that, the file holds it instead.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use tangible_api::secrets::SecretKey;
use tangible_domain::SecretString;

use crate::config::InitConfig;

/// The PostgreSQL password's file in the secrets directory.
pub const POSTGRES_PASSWORD: &str = "postgres-password";
/// The integration secret key's file in the secrets directory.
pub const SECRET_KEY: &str = "secret-key";

/// What became of one secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Made now.
    Made,
    /// There already, and left as it was.
    Kept,
    /// Written from the password given, which it was not before.
    Given,
}

/// What `tangible init` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// The PostgreSQL password.
    pub postgres_password: Outcome,
    /// The integration secret key.
    pub secret_key: Outcome,
}

/// `tangible init`.
///
/// # Errors
///
/// When the directory cannot be written, or a key there is not one.
pub fn init_command(config: &InitConfig) -> Result<()> {
    let report = init(&config.secrets_dir, config.postgres_password.as_ref())?;
    let said = |outcome: Outcome| match outcome {
        Outcome::Made => "made",
        Outcome::Kept => "kept",
        Outcome::Given => "set from POSTGRES_PASSWORD",
    };
    tracing::info!(
        dir = %config.secrets_dir.display(),
        postgres_password = said(report.postgres_password),
        secret_key = said(report.secret_key),
        "secrets ready"
    );
    Ok(())
}

/// Make what is missing in `dir`, keeping what is there.
///
/// # Errors
///
/// When a file cannot be read or written, or `secret-key` is not a key.
pub fn init(dir: &Path, postgres_password: Option<&SecretString>) -> Result<Report> {
    let given = postgres_password.filter(|password| !password.expose().is_empty());
    let password_path = dir.join(POSTGRES_PASSWORD);
    let postgres_password = match given {
        Some(password) => {
            let current = read_if_present(&password_path)?;
            if current.as_deref() == Some(password.expose()) {
                Outcome::Kept
            } else {
                write_whole(dir, POSTGRES_PASSWORD, password.expose(), 0o444)?;
                Outcome::Given
            }
        }
        None if password_path.exists() => Outcome::Kept,
        None => {
            let password = SecretKey::generate_password().context("making a password")?;
            write_whole(dir, POSTGRES_PASSWORD, password.expose(), 0o444)?;
            Outcome::Made
        }
    };

    let key_path = dir.join(SECRET_KEY);
    let secret_key = if key_path.exists() {
        // A key that does not open would only be found out once something
        // sealed with it is needed; say so now.
        SecretKey::from_file(&key_path)
            .map_err(|error| anyhow::anyhow!("{} is not a key: {error}", key_path.display()))?;
        Outcome::Kept
    } else {
        let key = SecretKey::generate().context("making a key")?;
        write_whole(dir, SECRET_KEY, key.expose(), 0o400)?;
        Outcome::Made
    };

    Ok(Report {
        postgres_password,
        secret_key,
    })
}

/// A file's text without its line ending, or none when it is absent.
fn read_if_present(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim_end_matches(['\r', '\n']).to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write `text` to `dir/name` with `mode`: whole, under a name of its own
/// first, then renamed over it, so a file there is never half written.
fn write_whole(dir: &Path, name: &str, text: &str, mode: u32) -> Result<()> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let part = dir.join(format!(".{name}.part"));
    let _ = fs::remove_file(&part);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode | 0o200)
        .open(&part)
        .with_context(|| format!("writing {}", part.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", part.display()))?;
    drop(file);
    // Writable only while it is written.
    fs::set_permissions(&part, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .with_context(|| format!("setting {}'s mode", part.display()))?;
    let path = dir.join(name);
    fs::rename(&part, &path).with_context(|| format!("putting {} in place", path.display()))?;
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing {}", dir.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tangible-init-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&dir).unwrap();
        dir
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_new_directory_gets_both_once_and_keeps_them() {
        let dir = scratch();
        let made = init(&dir, None).unwrap();
        assert_eq!(made.postgres_password, Outcome::Made);
        assert_eq!(made.secret_key, Outcome::Made);
        let password = fs::read_to_string(dir.join(POSTGRES_PASSWORD)).unwrap();
        assert_eq!(password.trim_end().len(), 48);
        assert!(password.trim_end().chars().all(|c| c.is_ascii_hexdigit()));
        SecretKey::from_file(&dir.join(SECRET_KEY)).unwrap();
        assert_eq!(mode(&dir.join(POSTGRES_PASSWORD)), 0o444);
        assert_eq!(mode(&dir.join(SECRET_KEY)), 0o400);

        let again = init(&dir, None).unwrap();
        assert_eq!(again.postgres_password, Outcome::Kept);
        assert_eq!(again.secret_key, Outcome::Kept);
        assert_eq!(
            fs::read_to_string(dir.join(POSTGRES_PASSWORD)).unwrap(),
            password
        );
        assert!(!dir.join(".postgres-password.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_older_installs_password_is_kept_and_empty_is_none() {
        let dir = scratch();
        let older = SecretString::from("the one in .env".to_owned());
        assert_eq!(
            init(&dir, Some(&older)).unwrap().postgres_password,
            Outcome::Given
        );
        assert_eq!(
            fs::read_to_string(dir.join(POSTGRES_PASSWORD)).unwrap(),
            "the one in .env\n"
        );
        assert_eq!(
            init(&dir, Some(&older)).unwrap().postgres_password,
            Outcome::Kept
        );
        // Compose passes an unset POSTGRES_PASSWORD as empty.
        let empty = SecretString::from(String::new());
        assert_eq!(
            init(&dir, Some(&empty)).unwrap().postgres_password,
            Outcome::Kept
        );
        assert_eq!(
            fs::read_to_string(dir.join(POSTGRES_PASSWORD)).unwrap(),
            "the one in .env\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_that_is_not_one_is_refused_and_left_alone() {
        let dir = scratch();
        fs::write(dir.join(SECRET_KEY), "not a key\n").unwrap();
        let error = init(&dir, None).unwrap_err().to_string();
        assert!(error.contains("is not a key"), "{error}");
        assert_eq!(
            fs::read_to_string(dir.join(SECRET_KEY)).unwrap(),
            "not a key\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
