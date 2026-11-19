// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `tangible init`: the secrets a new deployment needs, made once.
//!
//! compose.yaml's `init` service runs it before anything else
//! (docs/plans/install.md, I2), so that nobody has to invent a password
//! before the first `docker compose up`. It keeps two files, in two volumes:
//!
//! - `postgres-password`, in the database-password volume, which PostgreSQL
//!   is created with and the server, its migrations and the backup connect
//!   with. PostgreSQL's image reads it after dropping to its own user, so it
//!   is readable by anyone in the containers that mount the volume, which
//!   are those four.
//! - `secret-key`, in the secrets volume, which integration secrets are
//!   sealed with, readable by the server's user alone. Only the server
//!   mounts that volume, so a database's container never holds the key
//!   that opens what it stores (ADR 0016).
//!
//! Each is made only when absent, and never printed. An install from before
//! this keeps its `POSTGRES_PASSWORD` in `.env`, which its database was
//! created with; given that, the file holds it instead. 0.4.1 kept the
//! password in the secrets volume beside the key; found there, it is copied
//! (and left, so 0.4.1 still runs).
//!
//! Without `TANGIBLE_DATABASE_PASSWORD_DIR` the password stays in the
//! secrets directory, as 0.4.1 kept it. That is 0.4.1's compose.yaml, which
//! `docker compose pull` runs this image with: it mounts only the secrets
//! volume into `init`, on a read-only root, and points PostgreSQL and the
//! server at the password there.

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
    /// Copied from the secrets directory, where 0.4.1 kept it beside the
    /// key: the database was made with it.
    Copied,
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
    let database = config.password_dir();
    let report = init(
        &config.secrets_dir,
        database,
        config.postgres_password.as_ref(),
    )?;
    let said = |outcome: Outcome| match outcome {
        Outcome::Made => "made",
        Outcome::Kept => "kept",
        Outcome::Given => "set from POSTGRES_PASSWORD",
        Outcome::Copied => "copied from where 0.4.1 kept it",
    };
    tracing::info!(
        dir = %config.secrets_dir.display(),
        database_password_dir = %database.display(),
        postgres_password = said(report.postgres_password),
        secret_key = said(report.secret_key),
        "secrets ready"
    );
    Ok(())
}

/// Make what is missing, keeping what is there: the key in `secrets`, the
/// database's password in `database`.
///
/// # Errors
///
/// When a file cannot be read or written, or `secret-key` is not a key.
pub fn init(
    secrets: &Path,
    database: &Path,
    postgres_password: Option<&SecretString>,
) -> Result<Report> {
    let given = postgres_password.filter(|password| !password.expose().is_empty());
    let password_path = database.join(POSTGRES_PASSWORD);
    let postgres_password = match given {
        Some(password) => {
            let current = read_if_present(&password_path)?;
            if current.as_deref() == Some(password.expose()) {
                Outcome::Kept
            } else {
                write_whole(database, POSTGRES_PASSWORD, password.expose(), 0o444)?;
                Outcome::Given
            }
        }
        None if password_path.exists() => Outcome::Kept,
        None => match read_if_present(&secrets.join(POSTGRES_PASSWORD))? {
            Some(earlier) if !earlier.is_empty() => {
                write_whole(database, POSTGRES_PASSWORD, &earlier, 0o444)?;
                Outcome::Copied
            }
            _ => {
                let password = SecretKey::generate_password().context("making a password")?;
                write_whole(database, POSTGRES_PASSWORD, password.expose(), 0o444)?;
                Outcome::Made
            }
        },
    };

    let key_path = secrets.join(SECRET_KEY);
    let secret_key = if key_path.exists() {
        // A key that does not open would only be found out once something
        // sealed with it is needed; say so now.
        SecretKey::from_file(&key_path)
            .map_err(|error| anyhow::anyhow!("{} is not a key: {error}", key_path.display()))?;
        Outcome::Kept
    } else {
        let key = SecretKey::generate().context("making a key")?;
        write_whole(secrets, SECRET_KEY, key.expose(), 0o400)?;
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
    fn a_new_install_gets_both_once_each_in_its_own_place_and_keeps_them() {
        let (secrets, database) = (scratch(), scratch());
        let made = init(&secrets, &database, None).unwrap();
        assert_eq!(made.postgres_password, Outcome::Made);
        assert_eq!(made.secret_key, Outcome::Made);
        let password = fs::read_to_string(database.join(POSTGRES_PASSWORD)).unwrap();
        assert_eq!(password.trim_end().len(), 48);
        assert!(password.trim_end().chars().all(|c| c.is_ascii_hexdigit()));
        SecretKey::from_file(&secrets.join(SECRET_KEY)).unwrap();
        assert_eq!(mode(&database.join(POSTGRES_PASSWORD)), 0o444);
        assert_eq!(mode(&secrets.join(SECRET_KEY)), 0o400);
        // The key is not where the database's containers look, nor the
        // password beside the key.
        assert!(!database.join(SECRET_KEY).exists());
        assert!(!secrets.join(POSTGRES_PASSWORD).exists());

        let again = init(&secrets, &database, None).unwrap();
        assert_eq!(again.postgres_password, Outcome::Kept);
        assert_eq!(again.secret_key, Outcome::Kept);
        assert_eq!(
            fs::read_to_string(database.join(POSTGRES_PASSWORD)).unwrap(),
            password
        );
        assert!(!database.join(".postgres-password.part").exists());
        fs::remove_dir_all(&secrets).unwrap();
        fs::remove_dir_all(&database).unwrap();
    }

    #[test]
    fn an_older_installs_password_is_kept_and_empty_is_none() {
        let (secrets, database) = (scratch(), scratch());
        let older = SecretString::from("the one in .env".to_owned());
        assert_eq!(
            init(&secrets, &database, Some(&older))
                .unwrap()
                .postgres_password,
            Outcome::Given
        );
        assert_eq!(
            fs::read_to_string(database.join(POSTGRES_PASSWORD)).unwrap(),
            "the one in .env\n"
        );
        assert_eq!(
            init(&secrets, &database, Some(&older))
                .unwrap()
                .postgres_password,
            Outcome::Kept
        );
        // Compose passes an unset POSTGRES_PASSWORD as empty.
        let empty = SecretString::from(String::new());
        assert_eq!(
            init(&secrets, &database, Some(&empty))
                .unwrap()
                .postgres_password,
            Outcome::Kept
        );
        assert_eq!(
            fs::read_to_string(database.join(POSTGRES_PASSWORD)).unwrap(),
            "the one in .env\n"
        );
        fs::remove_dir_all(&secrets).unwrap();
        fs::remove_dir_all(&database).unwrap();
    }

    #[test]
    fn the_password_041_kept_beside_the_key_is_copied_and_left() {
        // 0.4.1 kept both in the secrets volume: its database was made with
        // that password, so it is the one the new place gets, and the old
        // file stays for 0.4.1 to go back to.
        let (secrets, database) = (scratch(), scratch());
        fs::write(secrets.join(POSTGRES_PASSWORD), "from 0.4.1\n").unwrap();
        let report = init(&secrets, &database, None).unwrap();
        assert_eq!(report.postgres_password, Outcome::Copied);
        assert_eq!(
            fs::read_to_string(database.join(POSTGRES_PASSWORD)).unwrap(),
            "from 0.4.1\n"
        );
        assert_eq!(mode(&database.join(POSTGRES_PASSWORD)), 0o444);
        assert_eq!(
            fs::read_to_string(secrets.join(POSTGRES_PASSWORD)).unwrap(),
            "from 0.4.1\n"
        );
        assert_eq!(
            init(&secrets, &database, None).unwrap().postgres_password,
            Outcome::Kept
        );
        // A password given in .env still wins over the copy.
        let given = SecretString::from("from .env".to_owned());
        let (secrets2, database2) = (scratch(), scratch());
        fs::write(secrets2.join(POSTGRES_PASSWORD), "from 0.4.1\n").unwrap();
        assert_eq!(
            init(&secrets2, &database2, Some(&given))
                .unwrap()
                .postgres_password,
            Outcome::Given
        );
        assert_eq!(
            fs::read_to_string(database2.join(POSTGRES_PASSWORD)).unwrap(),
            "from .env\n"
        );
        for dir in [secrets, database, secrets2, database2] {
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn under_041s_compose_file_the_password_stays_beside_the_key() {
        // 0.4.1's compose.yaml gives init the secrets volume alone, on a
        // read-only root: the password is found where it is and nothing is
        // written anywhere else.
        let secrets = scratch();
        fs::write(secrets.join(POSTGRES_PASSWORD), "from 0.4.1\n").unwrap();
        let first = init(&secrets, &secrets, None).unwrap();
        assert_eq!(first.postgres_password, Outcome::Kept);
        assert_eq!(first.secret_key, Outcome::Made);
        let mut names: Vec<_> = fs::read_dir(&secrets)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, [POSTGRES_PASSWORD, SECRET_KEY]);
        assert_eq!(
            fs::read_to_string(secrets.join(POSTGRES_PASSWORD)).unwrap(),
            "from 0.4.1\n"
        );
        // And an install from before 0.4.1, its password in .env, has it
        // written there, as 0.4.1 did.
        let older = scratch();
        let given = SecretString::from("the one in .env".to_owned());
        assert_eq!(
            init(&older, &older, Some(&given))
                .unwrap()
                .postgres_password,
            Outcome::Given
        );
        assert_eq!(
            fs::read_to_string(older.join(POSTGRES_PASSWORD)).unwrap(),
            "the one in .env\n"
        );
        for dir in [secrets, older] {
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_key_that_is_not_one_is_refused_and_left_alone() {
        let (secrets, database) = (scratch(), scratch());
        fs::write(secrets.join(SECRET_KEY), "not a key\n").unwrap();
        let error = init(&secrets, &database, None).unwrap_err().to_string();
        assert!(error.contains("is not a key"), "{error}");
        assert_eq!(
            fs::read_to_string(secrets.join(SECRET_KEY)).unwrap(),
            "not a key\n"
        );
        fs::remove_dir_all(&secrets).unwrap();
        fs::remove_dir_all(&database).unwrap();
    }
}
