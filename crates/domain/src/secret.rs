// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A string that must not reach a log.
//!
//! Passwords, bearer tokens, enrollment tokens and connection URLs with a
//! password in them travel through configuration, request bodies and worker
//! state. Each of those is a struct that derives `Debug`, and one `?value` in
//! a tracing call, an `expect` message or a test failure would print the
//! secret. Wrapping the value makes that impossible rather than merely
//! discouraged: `Debug` and `Display` both print `[redacted]`.
//!
//! It can be read from configuration and request bodies (`FromStr`,
//! `Deserialize`) but deliberately not written out: there is no `Serialize`.
//! A response or a state file that must carry a secret does so on purpose,
//! by calling [`SecretString::expose`] where it builds the output.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};

/// A secret string. Prints as `[redacted]`; read it with [`expose`].
///
/// [`expose`]: SecretString::expose
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a secret.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself.
    ///
    /// Named so that every use says out loud what it does. There is no
    /// `Deref` and no `as_str`, which would make a leak an accident rather
    /// than a decision.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether it is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl FromStr for SecretString {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.to_owned()))
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

/// Write `value` as `[redacted]` in a hand-written `Debug`.
///
/// For structs that must keep a secret as a plain `String`, because they
/// serialize it on purpose or are read straight from a database row:
/// `.field("token", &redacted(&self.token))`.
#[must_use]
pub fn redacted<T: ?Sized>(_value: &T) -> impl fmt::Debug {
    struct Redacted;
    impl fmt::Debug for Redacted {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("[redacted]")
        }
    }
    Redacted
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const VALUE: &str = "placeholder-value-for-redaction-test";

    #[test]
    fn neither_debug_nor_display_shows_the_value() {
        let secret = SecretString::new(VALUE);
        assert_eq!(format!("{secret:?}"), "[redacted]");
        assert_eq!(format!("{secret}"), "[redacted]");
        assert_eq!(format!("{:?}", Some(&secret)), "Some([redacted])");
        assert_eq!(secret.expose(), VALUE);
    }

    #[test]
    fn it_reads_from_json_and_from_text() {
        let from_json: SecretString = serde_json::from_str(&format!("\"{VALUE}\"")).unwrap();
        assert_eq!(from_json.expose(), VALUE);
        let from_text: SecretString = VALUE.parse().unwrap();
        assert_eq!(from_text, from_json);
    }

    #[test]
    fn a_hand_written_debug_can_redact_a_plain_field() {
        struct Holder {
            token: String,
        }
        impl fmt::Debug for Holder {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct("Holder")
                    .field("token", &redacted(&self.token))
                    .finish()
            }
        }
        let shown = format!(
            "{:?}",
            Holder {
                token: VALUE.to_owned()
            }
        );
        assert_eq!(shown, "Holder { token: [redacted] }");
    }
}
