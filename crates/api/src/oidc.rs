// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The server's side of signing in through an OpenID Connect provider
//! (ADR 0018, docs/plans/oidc.md O3): the provider, discovered when first
//! needed and kept, and the rules that turn its word into an account.
//!
//! The provider may be down when Tangible starts, so it is discovered at
//! the first sign-in that needs it, and a failure is tried again at the
//! next rather than remembered.

use std::sync::Arc;

use tangible_domain::Role;
use tangible_domain::auth::normalize_username;
use tangible_integrations::oidc::{Identity, OidcError, Provider, ProviderConfig};
use tangible_integrations::outbound::OutboundClient;
use tokio::sync::Mutex;

/// How long a sign-in may take between leaving for the provider and coming
/// back, in seconds.
pub const SIGN_IN_LIFETIME_SECONDS: i64 = 10 * 60;

/// The cookie that binds a sign-in under way to the browser that started it.
pub const BROWSER_COOKIE: &str = "tangible_oidc";

/// How the provider and its groups are configured.
#[derive(Debug, Clone)]
pub struct OidcSettings {
    /// The provider, and Tangible's registration there.
    pub provider: ProviderConfig,
    /// What the sign-in page calls it, such as `VoidAuth`.
    pub display_name: String,
    /// Groups whose members are administrators.
    pub admin_groups: Vec<String>,
    /// Groups whose members are operators.
    pub operator_groups: Vec<String>,
    /// Groups whose members are viewers.
    pub viewer_groups: Vec<String>,
}

impl OidcSettings {
    /// The highest role any of `groups` maps to, or `None`: nobody in a
    /// mapped group is refused, not given a role.
    #[must_use]
    pub fn role_for(&self, groups: &[String]) -> Option<Role> {
        let member = |mapped: &[String]| groups.iter().any(|group| mapped.contains(group));
        if member(&self.admin_groups) {
            Some(Role::Administrator)
        } else if member(&self.operator_groups) {
            Some(Role::Operator)
        } else if member(&self.viewer_groups) {
            Some(Role::Viewer)
        } else {
            None
        }
    }

    /// How many groups are mapped at all.
    #[must_use]
    pub fn mapped_groups(&self) -> usize {
        self.admin_groups.len() + self.operator_groups.len() + self.viewer_groups.len()
    }
}

/// The provider, discovered at first use.
pub struct Oidc {
    settings: OidcSettings,
    client: OutboundClient,
    provider: Mutex<Option<Arc<Provider>>>,
}

impl std::fmt::Debug for Oidc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Oidc")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Oidc {
    /// Sign-in through the provider these settings describe, reached through
    /// `client`.
    #[must_use]
    pub fn new(settings: OidcSettings, client: OutboundClient) -> Self {
        Self {
            settings,
            client,
            provider: Mutex::new(None),
        }
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> &OidcSettings {
        &self.settings
    }

    /// The provider, discovering it if it has not been.
    ///
    /// # Errors
    ///
    /// [`OidcError`] when it cannot be discovered; tried again next time.
    pub async fn provider(&self) -> Result<Arc<Provider>, OidcError> {
        let mut provider = self.provider.lock().await;
        if let Some(found) = provider.as_ref() {
            return Ok(Arc::clone(found));
        }
        let found = Arc::new(
            Provider::discover(self.client.clone(), self.settings.provider.clone()).await?,
        );
        tracing::info!(issuer = %found.issuer(), "identity provider discovered");
        *provider = Some(Arc::clone(&found));
        Ok(found)
    }
}

/// A path on Tangible to return to after signing in, from what the browser
/// asked for: a slash followed by neither a slash nor a backslash, with no
/// backslash or control character anywhere, as the catalogue also checks.
/// Anything else, or nothing, is `/`.
#[must_use]
pub fn safe_return_path(asked: Option<&str>) -> String {
    let Some(path) = asked else {
        return "/".to_owned();
    };
    let mut chars = path.chars();
    let starts_well = chars.next() == Some('/') && !matches!(chars.next(), Some('/' | '\\'));
    let clean = !path.chars().any(|c| c == '\\' || c.is_control());
    if starts_well && clean && path.len() <= 2048 {
        path.to_owned()
    } else {
        "/".to_owned()
    }
}

/// The username a new account for `identity` asks for: the provider's
/// preferred username, else the local part of its email, else `user`,
/// lowercased and with anything a username may not hold dropped.
#[must_use]
pub fn username_for(identity: &Identity) -> String {
    let wanted = identity
        .preferred_username
        .as_deref()
        .or_else(|| {
            identity
                .email
                .as_deref()
                .and_then(|email| email.split('@').next())
        })
        .unwrap_or("user");
    let cleaned: String = wanted
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == ' ' { '-' } else { c })
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(*c))
        .take(60)
        .collect();
    normalize_username(&cleaned).unwrap_or_else(|_| "user".to_owned())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn settings() -> OidcSettings {
        OidcSettings {
            provider: ProviderConfig {
                issuer: "https://id.example".parse().unwrap(),
                client_id: "tangible".to_owned(),
                client_secret: "secret".to_owned(),
                redirect_uri: "https://t.example/api/v1/session/oidc/callback"
                    .parse()
                    .unwrap(),
                scopes: vec![],
                groups_claim: "groups".to_owned(),
            },
            display_name: "VoidAuth".to_owned(),
            admin_groups: vec!["admins".to_owned()],
            operator_groups: vec!["ops".to_owned()],
            viewer_groups: vec!["family".to_owned()],
        }
    }

    fn groups(names: &[&str]) -> Vec<String> {
        names.iter().map(|&name| name.to_owned()).collect()
    }

    #[test]
    fn the_highest_mapped_role_wins_and_no_mapped_group_is_no_role() {
        let settings = settings();
        assert_eq!(
            settings.role_for(&groups(&["family", "ops"])),
            Some(Role::Operator)
        );
        assert_eq!(
            settings.role_for(&groups(&["admins", "family"])),
            Some(Role::Administrator)
        );
        assert_eq!(settings.role_for(&groups(&["family"])), Some(Role::Viewer));
        assert_eq!(settings.role_for(&groups(&["strangers"])), None);
        assert_eq!(settings.role_for(&[]), None);
        assert_eq!(settings.mapped_groups(), 3);
    }

    #[test]
    fn only_a_path_on_tangible_is_returned_to() {
        for good in ["/", "/library", "/library/a1?tab=x#y"] {
            assert_eq!(safe_return_path(Some(good)), good);
        }
        for bad in [
            "https://evil.example/",
            "//evil.example/",
            "/\\evil.example/",
            "/a\\b",
            "/a\nb",
            "library",
            "",
        ] {
            assert_eq!(safe_return_path(Some(bad)), "/", "{bad:?}");
        }
        assert_eq!(safe_return_path(None), "/");
        assert_eq!(
            safe_return_path(Some(&format!("/{}", "a".repeat(3000)))),
            "/"
        );
    }

    #[test]
    fn a_new_accounts_name_comes_from_the_provider_cleaned() {
        let identity = |preferred: Option<&str>, email: Option<&str>| Identity {
            issuer: "i".to_owned(),
            subject: "s".to_owned(),
            preferred_username: preferred.map(ToOwned::to_owned),
            email: email.map(ToOwned::to_owned),
            groups: vec![],
        };
        assert_eq!(
            username_for(&identity(Some("Alice Smith"), None)),
            "alice-smith"
        );
        assert_eq!(
            username_for(&identity(None, Some("Bob@example.com"))),
            "bob"
        );
        assert_eq!(username_for(&identity(None, None)), "user");
        assert_eq!(username_for(&identity(Some("ünïcödé"), None)), "ncd");
        assert_eq!(username_for(&identity(Some("ü"), None)), "user");
    }
}
