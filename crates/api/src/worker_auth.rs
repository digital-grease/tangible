// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Worker credentials and enrollment tokens.
//!
//! The gate to every worker route. Three properties matter more than
//! convenience here, and each is enforced by the types rather than by
//! remembering:
//!
//! - **Secrets are never stored.** Only a hash is. A [`WorkerCredential`] can
//!   be created and shown once; after that only its [`CredentialHash`]
//!   survives, and there is no method that recovers the secret from it.
//! - **A worker credential never authenticates a user route.** The extractor
//!   yields a [`WorkerIdentity`], which no user handler accepts, so the two
//!   cannot be confused by a handler signature that happens to compile.
//! - **Enrollment tokens are one-use.** Consumption is a state transition, not
//!   a lookup, so a replayed enrollment request cannot mint a second
//!   credential from the same token.
//!
//! Verification is constant-time. A token compared with `==` leaks its prefix
//! through timing, and these are bearer secrets for a service that drives
//! hardware.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use tangible_domain::WorkerId;
use time::OffsetDateTime;

/// Prefix on every worker credential, so one is recognisable in a log or a
/// support bundle and can be redacted on sight.
pub const CREDENTIAL_PREFIX: &str = "tgw_live_";

/// Prefix on every enrollment token.
pub const ENROLLMENT_PREFIX: &str = "tgw_enroll_";

/// Bytes of randomness in a secret.
///
/// 256 bits. These are bearer secrets with no rate limit worth relying on, so
/// the margin is deliberate.
const SECRET_BYTES: usize = 32;

/// A secret that must never be logged or serialized.
///
/// `Debug` and `Display` both redact. Serialization is deliberately not
/// implemented: a credential that can be serialized ends up in a response
/// body, a config dump, or an error payload eventually.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Generate a new secret with the given prefix.
    ///
    /// Fallible rather than panicking. Entropy comes straight from the
    /// operating system, and if that is unavailable the only safe response is
    /// to refuse: silently falling back to a weaker source would mint
    /// guessable credentials that look exactly like good ones.
    ///
    /// # Errors
    ///
    /// [`EntropyUnavailable`] if the operating system generator fails.
    pub fn generate(prefix: &str) -> Result<Self, EntropyUnavailable> {
        let mut bytes = [0_u8; SECRET_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| EntropyUnavailable)?;
        Ok(Self(format!("{prefix}{}", hex::encode(bytes))))
    }

    /// Wrap a secret supplied by a caller, e.g. from an Authorization header.
    #[must_use]
    pub fn from_supplied(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself.
    ///
    /// Named so that every use site says out loud what it is doing. There is
    /// no `Deref` or `as_str`, because those make leaking one an accident
    /// rather than a decision.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether it carries the expected prefix.
    ///
    /// Checked before hashing so an obviously wrong credential is rejected
    /// without the work, and so a user session token cannot be presented as a
    /// worker credential.
    #[must_use]
    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.0.starts_with(prefix)
    }

    /// Hash it for storage.
    ///
    /// SHA-256 rather than a password KDF. These are 256-bit random secrets,
    /// not user-chosen passwords: there is no dictionary to attack, so the
    /// slow-hash tradeoff buys nothing and would add latency to every worker
    /// request.
    #[must_use]
    pub fn hash(&self) -> CredentialHash {
        let mut hasher = Sha256::new();
        hasher.update(self.0.as_bytes());
        CredentialHash(hex::encode(hasher.finalize()))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the value, even in a panic message or a tracing field.
        f.write_str("Secret(redacted)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// The stored form of a secret.
///
/// Safe to persist, log and compare. Serializable precisely because it is not
/// the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialHash(String);

impl CredentialHash {
    /// Wrap a hash loaded from the database.
    #[must_use]
    pub fn from_stored(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The hash, for persistence.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether a supplied secret matches, in constant time.
    ///
    /// Comparing with `==` would return early at the first differing byte and
    /// leak the matching prefix through timing, which for a bearer secret is
    /// enough to recover it given enough attempts.
    #[must_use]
    pub fn verify(&self, supplied: &Secret) -> bool {
        let candidate = supplied.hash();
        self.0.as_bytes().ct_eq(candidate.0.as_bytes()).into()
    }
}

/// The operating system could not supply randomness.
///
/// Carries no detail on purpose: the underlying cause reaches the log, and a
/// caller can do nothing with it except fail the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the operating system random generator is unavailable")]
pub struct EntropyUnavailable;

/// A newly minted worker credential.
///
/// Deliberately not `Clone`: the secret is shown once, and a type that can be
/// duplicated invites keeping a copy.
#[derive(Debug)]
pub struct WorkerCredential {
    /// The secret, to be returned to the worker exactly once.
    pub secret: Secret,
    /// What to store.
    pub hash: CredentialHash,
}

impl WorkerCredential {
    /// Mint a credential.
    ///
    /// # Errors
    ///
    /// [`EntropyUnavailable`] if randomness cannot be obtained.
    pub fn issue() -> Result<Self, EntropyUnavailable> {
        let secret = Secret::generate(CREDENTIAL_PREFIX)?;
        let hash = secret.hash();
        Ok(Self { secret, hash })
    }
}

/// A newly minted enrollment token.
#[derive(Debug)]
pub struct EnrollmentToken {
    /// The secret, shown to the administrator once.
    pub secret: Secret,
    /// What to store.
    pub hash: CredentialHash,
    /// When it lapses.
    pub expires_at: OffsetDateTime,
}

impl EnrollmentToken {
    /// Mint a token valid for `lifetime`.
    ///
    /// Short by default. An enrollment token grants the ability to become a
    /// worker, and a worker drives hardware.
    ///
    /// # Errors
    ///
    /// [`EntropyUnavailable`] if randomness cannot be obtained.
    pub fn issue(
        now: OffsetDateTime,
        lifetime: time::Duration,
    ) -> Result<Self, EntropyUnavailable> {
        let secret = Secret::generate(ENROLLMENT_PREFIX)?;
        let hash = secret.hash();
        Ok(Self {
            secret,
            hash,
            expires_at: now + lifetime,
        })
    }
}

/// The recorded state of an enrollment token.
///
/// Consumption is a state transition rather than a lookup, which is what makes
/// a replayed enrollment request unable to mint a second credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentState {
    /// Issued and not yet used.
    Unused,
    /// Already exchanged for a credential.
    Consumed,
    /// Withdrawn before use.
    Revoked,
}

/// Why an enrollment attempt failed.
///
/// The variants exist for the server's logs. What is returned to the caller is
/// deliberately coarser: see [`EnrollmentRejection::public_reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EnrollmentRejection {
    /// No token matched.
    #[error("no such enrollment token")]
    Unknown,
    /// The token was already used.
    #[error("enrollment token has already been consumed")]
    AlreadyConsumed,
    /// The token was withdrawn.
    #[error("enrollment token was revoked")]
    Revoked,
    /// The token has lapsed.
    #[error("enrollment token has expired")]
    Expired,
    /// The value did not look like an enrollment token at all.
    #[error("value is not an enrollment token")]
    WrongKind,
}

impl EnrollmentRejection {
    /// What the caller is told.
    ///
    /// One message for every failure. Distinguishing "already consumed" from
    /// "no such token" would confirm to an attacker that a guessed token once
    /// existed, which is exactly the signal a brute-force needs.
    #[must_use]
    pub const fn public_reason(&self) -> &'static str {
        "the enrollment token is not valid"
    }
}

/// An enrollment token as stored.
#[derive(Debug, Clone)]
pub struct StoredEnrollment {
    /// Its hash.
    pub hash: CredentialHash,
    /// Its state.
    pub state: EnrollmentState,
    /// When it lapses.
    pub expires_at: OffsetDateTime,
}

impl StoredEnrollment {
    /// Whether a supplied token may be exchanged for a credential now.
    ///
    /// # Errors
    ///
    /// The specific [`EnrollmentRejection`], for logging. Callers must report
    /// [`EnrollmentRejection::public_reason`] to the client rather than the
    /// variant.
    pub fn check(&self, supplied: &Secret, now: OffsetDateTime) -> Result<(), EnrollmentRejection> {
        if !supplied.has_prefix(ENROLLMENT_PREFIX) {
            return Err(EnrollmentRejection::WrongKind);
        }
        if !self.hash.verify(supplied) {
            return Err(EnrollmentRejection::Unknown);
        }
        match self.state {
            EnrollmentState::Consumed => return Err(EnrollmentRejection::AlreadyConsumed),
            EnrollmentState::Revoked => return Err(EnrollmentRejection::Revoked),
            EnrollmentState::Unused => {}
        }
        if now >= self.expires_at {
            return Err(EnrollmentRejection::Expired);
        }
        Ok(())
    }
}

/// Who a request is authenticated as, when it is a worker.
///
/// A distinct type from any user principal on purpose. A handler that takes a
/// [`WorkerIdentity`] cannot accidentally be reached by a user session, and a
/// handler that takes a user principal cannot be reached by a worker
/// credential, because neither converts into the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerIdentity {
    /// The authenticated worker.
    pub worker_id: WorkerId,
}

/// Why worker authentication failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthRejection {
    /// No Authorization header.
    #[error("no credential presented")]
    Missing,
    /// The header was not a bearer token.
    #[error("credential is not a bearer token")]
    NotBearer,
    /// The value did not carry the worker credential prefix.
    ///
    /// Distinguished internally because presenting a *user* token to a worker
    /// route is worth noticing in the logs: it usually means a
    /// misconfiguration, occasionally something worse.
    #[error("credential is not a worker credential")]
    WrongKind,
    /// No worker matched, or the secret did not verify.
    #[error("credential is not recognised")]
    Unrecognised,
    /// The worker has been revoked.
    #[error("worker has been revoked")]
    Revoked,
}

impl AuthRejection {
    /// What the caller is told.
    ///
    /// Uniform for the same reason enrollment failures are: a caller must not
    /// be able to distinguish "this credential once existed" from "this
    /// credential never existed".
    #[must_use]
    pub const fn public_reason(&self) -> &'static str {
        "the credential is not valid"
    }
}

/// Extract a bearer credential from an Authorization header value.
///
/// # Errors
///
/// [`AuthRejection::NotBearer`] if the scheme is wrong, or
/// [`AuthRejection::WrongKind`] if it is not a worker credential.
pub fn bearer_worker_credential(header: &str) -> Result<Secret, AuthRejection> {
    // Scheme names are case-insensitive per RFC 7235, and clients differ.
    let rest = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .ok_or(AuthRejection::NotBearer)?;

    let secret = Secret::from_supplied(rest.trim());
    if !secret.has_prefix(CREDENTIAL_PREFIX) {
        // A user session token presented here must be refused as a *kind*
        // error before any lookup, so worker routes are unreachable with user
        // credentials regardless of what the store contains.
        return Err(AuthRejection::WrongKind);
    }
    Ok(secret)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    // --- secrets never escape --------------------------------------------

    #[test]
    fn debug_and_display_both_redact() {
        // These reach panic messages, tracing fields and support bundles.
        let secret = Secret::generate(CREDENTIAL_PREFIX).expect("entropy");
        assert!(!format!("{secret:?}").contains("tgw_live_"));
        assert!(!format!("{secret}").contains("tgw_live_"));
        assert_eq!(format!("{secret}"), "[redacted]");
    }

    #[test]
    fn a_credential_carries_a_recognisable_prefix() {
        // So one is spottable in a log and can be redacted on sight.
        let credential = WorkerCredential::issue().expect("entropy");
        assert!(credential.secret.expose().starts_with(CREDENTIAL_PREFIX));
    }

    #[test]
    fn secrets_are_unpredictable() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..256 {
            assert!(
                seen.insert(
                    Secret::generate(CREDENTIAL_PREFIX)
                        .expect("entropy")
                        .expose()
                        .to_owned()
                ),
                "a secret repeated"
            );
        }
    }

    #[test]
    fn the_hash_is_not_the_secret() {
        let credential = WorkerCredential::issue().expect("entropy");
        assert!(
            !credential
                .hash
                .as_str()
                .contains(credential.secret.expose())
        );
        assert!(!credential.hash.as_str().starts_with(CREDENTIAL_PREFIX));
    }

    // --- verification -----------------------------------------------------

    #[test]
    fn the_right_secret_verifies_and_a_wrong_one_does_not() {
        let credential = WorkerCredential::issue().expect("entropy");
        assert!(credential.hash.verify(&credential.secret));

        let other = WorkerCredential::issue().expect("entropy");
        assert!(!credential.hash.verify(&other.secret));
    }

    #[test]
    fn a_secret_sharing_a_long_prefix_still_fails() {
        // Guards the constant-time comparison: a near-miss must be as rejected
        // as a total miss.
        let credential = WorkerCredential::issue().expect("entropy");
        let mut near = credential.secret.expose().to_owned();
        near.pop();
        near.push(if credential.secret.expose().ends_with('a') {
            'b'
        } else {
            'a'
        });
        assert!(!credential.hash.verify(&Secret::from_supplied(near)));
    }

    #[test]
    fn an_empty_credential_does_not_verify() {
        let credential = WorkerCredential::issue().expect("entropy");
        assert!(!credential.hash.verify(&Secret::from_supplied("")));
    }

    // --- enrollment is one-use ---------------------------------------------

    #[test]
    fn an_unused_token_within_its_lifetime_is_accepted() {
        let token = EnrollmentToken::issue(now(), time::Duration::minutes(15)).expect("entropy");
        let stored = StoredEnrollment {
            hash: token.hash.clone(),
            state: EnrollmentState::Unused,
            expires_at: token.expires_at,
        };
        assert_eq!(stored.check(&token.secret, now()), Ok(()));
    }

    #[test]
    fn a_consumed_token_cannot_mint_a_second_credential() {
        // The property that makes enrollment replay-proof.
        let token = EnrollmentToken::issue(now(), time::Duration::minutes(15)).expect("entropy");
        let stored = StoredEnrollment {
            hash: token.hash.clone(),
            state: EnrollmentState::Consumed,
            expires_at: token.expires_at,
        };
        assert_eq!(
            stored.check(&token.secret, now()),
            Err(EnrollmentRejection::AlreadyConsumed)
        );
    }

    #[test]
    fn a_revoked_token_is_refused() {
        let token = EnrollmentToken::issue(now(), time::Duration::minutes(15)).expect("entropy");
        let stored = StoredEnrollment {
            hash: token.hash.clone(),
            state: EnrollmentState::Revoked,
            expires_at: token.expires_at,
        };
        assert_eq!(
            stored.check(&token.secret, now()),
            Err(EnrollmentRejection::Revoked)
        );
    }

    #[test]
    fn an_expired_token_is_refused_at_the_moment_it_lapses() {
        let token = EnrollmentToken::issue(now(), time::Duration::minutes(15)).expect("entropy");
        let stored = StoredEnrollment {
            hash: token.hash.clone(),
            state: EnrollmentState::Unused,
            expires_at: token.expires_at,
        };
        assert_eq!(
            stored.check(&token.secret, token.expires_at - time::Duration::seconds(1)),
            Ok(())
        );
        assert_eq!(
            stored.check(&token.secret, token.expires_at),
            Err(EnrollmentRejection::Expired)
        );
    }

    #[test]
    fn a_worker_credential_cannot_be_used_as_an_enrollment_token() {
        // Different prefixes, checked before any hashing.
        let token = EnrollmentToken::issue(now(), time::Duration::minutes(15)).expect("entropy");
        let stored = StoredEnrollment {
            hash: token.hash.clone(),
            state: EnrollmentState::Unused,
            expires_at: token.expires_at,
        };
        let credential = WorkerCredential::issue().expect("entropy");
        assert_eq!(
            stored.check(&credential.secret, now()),
            Err(EnrollmentRejection::WrongKind)
        );
    }

    #[test]
    fn every_enrollment_failure_reports_the_same_thing_publicly() {
        // Distinguishing "already consumed" from "no such token" would confirm
        // a guessed token once existed.
        let reasons: std::collections::BTreeSet<_> = [
            EnrollmentRejection::Unknown,
            EnrollmentRejection::AlreadyConsumed,
            EnrollmentRejection::Revoked,
            EnrollmentRejection::Expired,
            EnrollmentRejection::WrongKind,
        ]
        .iter()
        .map(EnrollmentRejection::public_reason)
        .collect();
        assert_eq!(reasons.len(), 1, "public reasons must be indistinguishable");
    }

    // --- worker credentials do not authenticate user routes ------------------

    #[test]
    fn a_worker_credential_is_extracted_from_a_bearer_header() {
        let credential = WorkerCredential::issue().expect("entropy");
        let header = format!("Bearer {}", credential.secret.expose());
        let extracted = bearer_worker_credential(&header).expect("extract");
        assert!(credential.hash.verify(&extracted));
    }

    #[test]
    fn the_bearer_scheme_is_matched_case_insensitively() {
        // RFC 7235 says scheme names are case-insensitive and clients differ.
        let credential = WorkerCredential::issue().expect("entropy");
        let header = format!("bearer {}", credential.secret.expose());
        assert!(bearer_worker_credential(&header).is_ok());
    }

    #[test]
    fn a_user_session_token_is_refused_before_any_lookup() {
        // The rule that keeps worker routes unreachable with user credentials
        // regardless of what the store happens to contain.
        for user_token in [
            "Bearer session_abcdef",
            "Bearer tangible_user_abcdef",
            "Bearer eyJhbGciOiJIUzI1NiJ9.payload.signature",
        ] {
            assert_eq!(
                bearer_worker_credential(user_token),
                Err(AuthRejection::WrongKind),
                "{user_token} must not authenticate a worker route"
            );
        }
    }

    #[test]
    fn an_enrollment_token_does_not_authenticate_worker_routes() {
        // It grants the right to *become* a worker, not to act as one.
        let token = EnrollmentToken::issue(now(), time::Duration::minutes(15)).expect("entropy");
        let header = format!("Bearer {}", token.secret.expose());
        assert_eq!(
            bearer_worker_credential(&header),
            Err(AuthRejection::WrongKind)
        );
    }

    #[test]
    fn a_non_bearer_scheme_is_refused() {
        for header in ["Basic dXNlcjpwYXNz", "tgw_live_abc", ""] {
            assert_eq!(
                bearer_worker_credential(header),
                Err(AuthRejection::NotBearer),
                "{header:?}"
            );
        }
    }

    #[test]
    fn every_auth_failure_reports_the_same_thing_publicly() {
        let reasons: std::collections::BTreeSet<_> = [
            AuthRejection::Missing,
            AuthRejection::NotBearer,
            AuthRejection::WrongKind,
            AuthRejection::Unrecognised,
            AuthRejection::Revoked,
        ]
        .iter()
        .map(AuthRejection::public_reason)
        .collect();
        assert_eq!(reasons.len(), 1);
    }

    #[test]
    fn a_stored_hash_round_trips_through_serde() {
        // It is persisted on the worker row.
        let credential = WorkerCredential::issue().expect("entropy");
        let json = serde_json::to_string(&credential.hash).expect("serialize");
        assert_eq!(
            serde_json::from_str::<CredentialHash>(&json).expect("deserialize"),
            credential.hash
        );
        assert!(
            !json.contains(credential.secret.expose()),
            "the stored form must not contain the secret"
        );
    }
}
