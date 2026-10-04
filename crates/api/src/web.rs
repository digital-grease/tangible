// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The web UI, served from the same origin as the API.
//!
//! The UI is a static single-page build: every page loads its data in the
//! browser through the public API, exactly as any other client does. Serving
//! those files from this process keeps one origin, so the session cookie and
//! the anti-forgery token need no cross-origin configuration, and keeps the
//! deployment to one container with no Node runtime in it.
//!
//! Anything the router does not match falls through to here. API paths that
//! match nothing still get a JSON 404, never the UI's page, so a client that
//! mistypes a route is told so rather than handed HTML.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt as _;
use tower_http::services::{ServeDir, ServeFile};

use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// The page every route of the UI starts from.
const INDEX: &str = "index.html";

/// Largest index page read at startup. The real one is a few kilobytes.
const MAX_INDEX_BYTES: u64 = 1024 * 1024;

/// Where the build puts files whose names carry their content hash.
const IMMUTABLE_PREFIX: &str = "/_app/immutable/";

/// What the server sends the UI's pages instead of the API's policy.
///
/// Used only if the build carries no policy of its own, which a correct build
/// always does. Still strict: it forbids framing and plugins, and the page's
/// scripts will not run without their hashes, so a broken build fails visibly
/// rather than running unprotected.
const FALLBACK_CSP: &str = "default-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

/// A built UI, ready to serve.
#[derive(Debug, Clone)]
pub struct WebUi {
    root: PathBuf,
    csp: String,
}

/// Why a UI could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum WebUiError {
    /// The index page is missing or unreadable.
    #[error("could not read {path}: {source}")]
    Index {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },
    /// The index page is implausibly large.
    #[error("{path} is larger than {MAX_INDEX_BYTES} bytes; is this a Tangible web build?")]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
}

impl WebUi {
    /// Open the build in `root`, which must contain `index.html`.
    ///
    /// The page's own Content Security Policy, which the build writes with
    /// the hashes of its inline bootstrap, is read once here and sent as a
    /// header too, with `frame-ancestors`, which only a header can carry.
    ///
    /// # Errors
    ///
    /// [`WebUiError`] if the index page cannot be read.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, WebUiError> {
        let root = root.into();
        let index = root.join(INDEX);
        let metadata = tokio::fs::metadata(&index)
            .await
            .map_err(|source| WebUiError::Index {
                path: index.clone(),
                source,
            })?;
        if metadata.len() > MAX_INDEX_BYTES {
            return Err(WebUiError::TooLarge { path: index });
        }
        let page = tokio::fs::read_to_string(&index)
            .await
            .map_err(|source| WebUiError::Index {
                path: index.clone(),
                source,
            })?;
        let csp = page_policy(&page).map_or_else(
            || {
                tracing::warn!(
                    page = %index.display(),
                    "the web UI's page carries no Content Security Policy; using a strict default"
                );
                FALLBACK_CSP.to_owned()
            },
            |policy| format!("{policy}; frame-ancestors 'none'"),
        );
        Ok(Self { root, csp })
    }

    /// The directory being served.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The policy sent with the UI's responses.
    #[must_use]
    pub fn csp(&self) -> &str {
        &self.csp
    }
}

/// The policy a page declares in its `<meta http-equiv>` tag, if it has one.
fn page_policy(page: &str) -> Option<String> {
    let lower = page.to_ascii_lowercase();
    let tag_start = lower.find("<meta http-equiv=\"content-security-policy\"")?;
    let tag_end = tag_start + lower[tag_start..].find('>')?;
    let tag = &page[tag_start..tag_end];
    let marker = "content=\"";
    let content_start = tag.to_ascii_lowercase().find(marker)? + marker.len();
    let content_end = content_start + tag[content_start..].find('"')?;
    let policy = tag[content_start..content_end].trim();
    // A header value cannot carry control characters.
    (!policy.is_empty() && !policy.chars().any(char::is_control)).then(|| policy.to_owned())
}

/// Whether a path belongs to the API rather than the UI.
#[must_use]
pub fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/") || path == "/livez" || path == "/readyz"
}

/// Whether a path names a file whose name carries its content hash, which a
/// browser may therefore keep for as long as it likes.
#[must_use]
pub fn is_immutable_asset(path: &str) -> bool {
    path.starts_with(IMMUTABLE_PREFIX)
}

/// Answer anything the router did not match.
pub async fn fallback(State(state): State<ApiState>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if is_api_path(&path) {
        return Problem::new(ErrorCode::NotFound, format!("no route {path}")).into_response();
    }
    let Some(web) = state.web() else {
        return Problem::new(
            ErrorCode::NotFound,
            "the web UI is not installed on this server; the API is under /api/v1",
        )
        .into_response();
    };

    // A missing asset is a 404, not the UI's page: a stale tab asking for a
    // script from an older build should fail as itself, not as HTML that the
    // browser then refuses to run as a script.
    let served = if is_immutable_asset(&path) {
        ServeDir::new(web.root()).oneshot(request).await
    } else {
        ServeDir::new(web.root())
            .fallback(ServeFile::new(web.root().join(INDEX)))
            .oneshot(request)
            .await
    };
    match served {
        Ok(response) => response.map(Body::new),
        Err(never) => match never {},
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_is_read_from_the_page() {
        let page = r#"<html><head><meta charset="utf-8" /><meta http-equiv="content-security-policy" content="default-src 'self'; script-src 'self' 'sha256-abc='"></head></html>"#;
        assert_eq!(
            page_policy(page).as_deref(),
            Some("default-src 'self'; script-src 'self' 'sha256-abc='")
        );
    }

    #[test]
    fn a_page_without_a_policy_has_none() {
        assert!(page_policy("<html><head></head></html>").is_none());
        assert!(page_policy(r#"<meta http-equiv="content-security-policy" content="">"#).is_none());
    }

    #[test]
    fn api_paths_are_told_apart_from_the_ui() {
        for path in ["/api", "/api/v1/nothing", "/livez", "/readyz"] {
            assert!(is_api_path(path), "{path}");
        }
        for path in [
            "/",
            "/library",
            "/apiary",
            "/burns/abc",
            "/_app/immutable/x.js",
        ] {
            assert!(!is_api_path(path), "{path}");
        }
    }

    #[test]
    fn only_hashed_assets_are_immutable() {
        assert!(is_immutable_asset("/_app/immutable/entry/start.abc.js"));
        assert!(!is_immutable_asset("/_app/version.json"));
        assert!(!is_immutable_asset("/index.html"));
    }

    #[tokio::test]
    async fn opening_a_build_sends_its_policy_with_frame_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(INDEX),
            r#"<meta http-equiv="content-security-policy" content="default-src 'self'">"#,
        )
        .unwrap();
        let web = WebUi::open(dir.path()).await.unwrap();
        assert_eq!(web.csp(), "default-src 'self'; frame-ancestors 'none'");
    }

    #[tokio::test]
    async fn a_build_without_a_policy_gets_the_strict_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(INDEX), "<html></html>").unwrap();
        let web = WebUi::open(dir.path()).await.unwrap();
        assert_eq!(web.csp(), FALLBACK_CSP);
    }

    #[tokio::test]
    async fn a_directory_without_a_page_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            WebUi::open(dir.path()).await,
            Err(WebUiError::Index { .. })
        ));
    }
}
