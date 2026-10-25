// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integration secrets, encrypted in the database (ADR 0016).
//!
//! A download client's password or an API key is entered once, in the web UI,
//! and never shown again. It is stored sealed with ChaCha20-Poly1305 under a
//! key that lives outside the database, in the file `TANGIBLE_SECRET_KEY_FILE`
//! names: a stolen database without that file holds nothing usable.
//!
//! - The key is 32 random bytes, written base64 in the file.
//! - Each secret gets a fresh random 96-bit nonce.
//! - The integration's id and the field's name are the additional data, so a
//!   sealed value moved to another row or field fails to open rather than
//!   becoming that row's secret.
//! - Each sealed value records the key's id (the first 8 bytes of its
//!   SHA-256, in hex), so a wrong key is told from corrupt data, and
//!   `tangible secrets rotate` can re-seal every value under a new key.
//!
//! Through `ring`, already in the tree through rustls.

use std::fmt;
use std::path::Path;

use base64::Engine as _;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom as _, SystemRandom};
use sha2::{Digest as _, Sha256};
use tangible_db::integrations::SealedRow;
use tangible_domain::SecretString;

/// A sealed secret as stored.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    /// The id of the key it was sealed with.
    pub key_id: String,
    /// Its nonce.
    pub nonce: Vec<u8>,
    /// The ciphertext with its tag.
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// Why a secret could not be sealed or opened, or the key not loaded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// The key file could not be read.
    #[error("the secret key file could not be read: {0}")]
    Unreadable(String),
    /// The key file does not hold 32 bytes of base64.
    #[error("the secret key file must hold 32 random bytes, written base64")]
    Malformed,
    /// Sealed under another key.
    #[error("sealed under key {found}, and this server holds key {held}")]
    WrongKey {
        /// The key it was sealed with.
        found: String,
        /// The key this server has.
        held: String,
    },
    /// It did not open: corrupt, or moved to another row or field.
    #[error("a stored secret did not open; it is corrupt or not this field's")]
    Corrupt,
    /// The system's random source failed.
    #[error("no random bytes were available")]
    Random,
}

/// The key integration secrets are sealed with.
pub struct SecretKey {
    key: LessSafeKey,
    id: String,
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SecretKey {
    /// The key in the file at `path`.
    ///
    /// # Errors
    ///
    /// [`SecretError::Unreadable`] or [`SecretError::Malformed`].
    pub fn from_file(path: &Path) -> Result<Self, SecretError> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| SecretError::Unreadable(error.kind().to_string()))?;
        Self::from_base64(&SecretString::from(text))
    }

    /// The key written base64.
    ///
    /// # Errors
    ///
    /// [`SecretError::Malformed`] unless it is 32 bytes.
    pub fn from_base64(text: &SecretString) -> Result<Self, SecretError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text.expose().trim())
            .map_err(|_| SecretError::Malformed)?;
        Self::from_bytes(&bytes)
    }

    /// The key from its raw bytes.
    ///
    /// # Errors
    ///
    /// [`SecretError::Malformed`] unless there are 32 of them.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SecretError> {
        if bytes.len() != 32 {
            return Err(SecretError::Malformed);
        }
        let unbound =
            UnboundKey::new(&CHACHA20_POLY1305, bytes).map_err(|_| SecretError::Malformed)?;
        let digest = Sha256::digest(bytes);
        Ok(Self {
            key: LessSafeKey::new(unbound),
            id: hex::encode(&digest[..8]),
        })
    }

    /// A new random key, written base64, for an administrator to keep.
    ///
    /// # Errors
    ///
    /// [`SecretError::Random`] when the system has no randomness to give.
    pub fn generate() -> Result<SecretString, SecretError> {
        let mut bytes = [0_u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| SecretError::Random)?;
        Ok(SecretString::from(
            base64::engine::general_purpose::STANDARD.encode(bytes),
        ))
    }

    /// This key's id: the first 8 bytes of its SHA-256, in hex. Not secret.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Seal `secret` for `field` of integration `integration_id`.
    ///
    /// # Errors
    ///
    /// [`SecretError::Random`] when no nonce could be drawn.
    pub fn seal(
        &self,
        integration_id: &str,
        field: &str,
        secret: &SecretString,
    ) -> Result<Sealed, SecretError> {
        let mut nonce = [0_u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| SecretError::Random)?;
        let mut in_out = secret.expose().as_bytes().to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(additional_data(integration_id, field)),
                &mut in_out,
            )
            .map_err(|_| SecretError::Corrupt)?;
        Ok(Sealed {
            key_id: self.id.clone(),
            nonce: nonce.to_vec(),
            ciphertext: in_out,
        })
    }

    /// Open a secret sealed for `field` of integration `integration_id`.
    ///
    /// # Errors
    ///
    /// [`SecretError::WrongKey`] for one sealed under another key;
    /// [`SecretError::Corrupt`] for one that does not open.
    pub fn open(
        &self,
        integration_id: &str,
        field: &str,
        sealed: &Sealed,
    ) -> Result<SecretString, SecretError> {
        if sealed.key_id != self.id {
            return Err(SecretError::WrongKey {
                found: sealed.key_id.clone(),
                held: self.id.clone(),
            });
        }
        let nonce =
            Nonce::try_assume_unique_for_key(&sealed.nonce).map_err(|_| SecretError::Corrupt)?;
        let mut in_out = sealed.ciphertext.clone();
        let plain = self
            .key
            .open_in_place(
                nonce,
                Aad::from(additional_data(integration_id, field)),
                &mut in_out,
            )
            .map_err(|_| SecretError::Corrupt)?;
        String::from_utf8(plain.to_vec())
            .map(SecretString::from)
            .map_err(|_| SecretError::Corrupt)
    }
}

impl From<&SealedRow> for Sealed {
    fn from(row: &SealedRow) -> Self {
        Self {
            key_id: row.key_id.clone(),
            nonce: row.nonce.clone(),
            ciphertext: row.ciphertext.clone(),
        }
    }
}

/// Open a stored secret with `old` and seal it with `new`, for
/// `tangible secrets rotate`. One already sealed with `new` is answered as
/// it is, so a rotation can be run again.
///
/// # Errors
///
/// [`SecretError::WrongKey`] for one sealed with neither key;
/// [`SecretError::Corrupt`] for one that does not open.
pub fn reseal(old: &SecretKey, new: &SecretKey, row: &SealedRow) -> Result<SealedRow, SecretError> {
    if row.key_id == new.id {
        return Ok(row.clone());
    }
    let id = row.integration_id.to_string();
    let secret = old.open(&id, &row.field, &Sealed::from(row))?;
    let sealed = new.seal(&id, &row.field, &secret)?;
    Ok(SealedRow {
        key_id: sealed.key_id,
        nonce: sealed.nonce,
        ciphertext: sealed.ciphertext,
        ..row.clone()
    })
}

/// What binds a sealed value to its row and field.
fn additional_data(integration_id: &str, field: &str) -> Vec<u8> {
    format!("tangible-integration-secret/v1\0{integration_id}\0{field}").into_bytes()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn key(byte: u8) -> SecretKey {
        SecretKey::from_bytes(&[byte; 32]).unwrap()
    }

    #[test]
    fn a_secret_opens_only_for_its_row_and_field_under_its_key() {
        let key = key(7);
        let secret = SecretString::from("hunter2".to_owned());
        let sealed = key.seal("i1", "password", &secret).unwrap();
        assert_eq!(
            key.open("i1", "password", &sealed).unwrap().expose(),
            "hunter2"
        );
        assert!(!sealed.ciphertext.windows(7).any(|w| w == b"hunter2"));
        assert_eq!(
            key.open("i2", "password", &sealed),
            Err(SecretError::Corrupt)
        );
        assert_eq!(
            key.open("i1", "api_key", &sealed),
            Err(SecretError::Corrupt)
        );
        let mut tampered = sealed.clone();
        tampered.ciphertext[0] ^= 1;
        assert_eq!(
            key.open("i1", "password", &tampered),
            Err(SecretError::Corrupt)
        );
        assert!(matches!(
            self::key(8).open("i1", "password", &sealed),
            Err(SecretError::WrongKey { .. })
        ));
    }

    #[test]
    fn every_seal_draws_a_new_nonce() {
        let key = key(1);
        let secret = SecretString::from("same".to_owned());
        let a = key.seal("i", "f", &secret).unwrap();
        let b = key.seal("i", "f", &secret).unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn a_key_is_32_bytes_of_base64_and_its_id_is_not_the_key() {
        let generated = SecretKey::generate().unwrap();
        let key = SecretKey::from_base64(&generated).unwrap();
        assert_eq!(key.id().len(), 16);
        assert!(!generated.expose().contains(key.id()));
        for bad in ["", "not base64!", "AAAA"] {
            assert!(
                SecretKey::from_base64(&SecretString::from(bad.to_owned())).is_err(),
                "{bad:?}"
            );
        }
        assert!(!format!("{key:?}").contains(generated.expose()));
        let sealed = key
            .seal("i", "f", &SecretString::from("x".to_owned()))
            .unwrap();
        assert!(!format!("{sealed:?}").contains("ciphertext"));
    }

    #[test]
    fn rotation_reseals_under_the_new_key_and_leaves_what_it_already_did() {
        let (old, new) = (key(1), key(2));
        let id = uuid::Uuid::now_v7();
        let sealed = old
            .seal(
                &id.to_string(),
                "password",
                &SecretString::from("pw".to_owned()),
            )
            .unwrap();
        let row = SealedRow {
            integration_id: id,
            field: "password".to_owned(),
            key_id: sealed.key_id,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
        };
        let resealed = reseal(&old, &new, &row).unwrap();
        assert_eq!(resealed.key_id, new.id());
        assert_eq!(
            new.open(&id.to_string(), "password", &Sealed::from(&resealed))
                .unwrap()
                .expose(),
            "pw"
        );
        assert_eq!(reseal(&old, &new, &resealed).unwrap(), resealed);
        assert!(matches!(
            reseal(&key(3), &new, &row),
            Err(SecretError::WrongKey { .. })
        ));
    }

    #[test]
    fn a_key_is_read_from_its_file_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        let generated = SecretKey::generate().unwrap();
        std::fs::write(&path, format!("{}\n", generated.expose())).unwrap();
        let key = SecretKey::from_file(&path).unwrap();
        assert_eq!(key.id(), SecretKey::from_base64(&generated).unwrap().id());
        assert!(matches!(
            SecretKey::from_file(&dir.path().join("missing")),
            Err(SecretError::Unreadable(_))
        ));
    }
}
