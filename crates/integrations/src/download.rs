// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What the download clients have in common (docs/plans/acquisition.md A4,
//! A5): how one fails, where a job stands, how a path it reports is mapped
//! onto a watched root, and reading its answers within a bound. qBittorrent,
//! SABnzbd and NZBGet each speak their own API on top.

use crate::outbound::{OutboundError, Refusal};

/// The most of a response body read from a client.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Why a client could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The address is one the policy refuses.
    #[error("the address will not be reached: {0}")]
    Refused(Refusal),
    /// Nothing answered, or the connection failed.
    #[error("{client} could not be reached: {detail}")]
    Unreachable {
        /// Which client, in words.
        client: &'static str,
        /// What went wrong, without the URL.
        detail: String,
    },
    /// Signing in, or the key, was refused.
    #[error("{client} refused to sign in: {reason}")]
    Auth {
        /// Which client, in words.
        client: &'static str,
        /// Why, as far as can be told.
        reason: &'static str,
    },
    /// It answered with something Tangible cannot use.
    #[error("{client} answered unexpectedly: {detail}")]
    BadResponse {
        /// Which client, in words.
        client: &'static str,
        /// What was wrong with the answer.
        detail: String,
    },
}

impl ClientError {
    /// The stable code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Refused(_) => "INTEGRATION_ADDRESS_REFUSED",
            Self::Unreachable { .. } => "INTEGRATION_UNREACHABLE",
            Self::Auth { .. } => "INTEGRATION_AUTH_FAILED",
            Self::BadResponse { .. } => "INTEGRATION_BAD_RESPONSE",
        }
    }

    /// A refusal or transport failure from the outbound client.
    #[must_use]
    pub fn from_outbound(client: &'static str, error: OutboundError) -> Self {
        match error {
            OutboundError::Refused(refusal) => Self::Refused(refusal),
            OutboundError::Transport(error) => Self::Unreachable {
                client,
                detail: error.to_string(),
            },
        }
    }

    /// An answer that will not do.
    #[must_use]
    pub fn bad(client: &'static str, detail: impl Into<String>) -> Self {
        Self::BadResponse {
            client,
            detail: detail.into(),
        }
    }
}

/// Where a client's job stands, for an import waiting on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Still coming: so many bytes of so many, once known.
    Downloading {
        /// Bytes it has.
        done: Option<i64>,
        /// Bytes it will have.
        total: Option<i64>,
    },
    /// All there, at this path as the client sees it.
    Complete {
        /// The file or folder, absolute, as the client reports it.
        content_path: String,
        /// Its size.
        size: i64,
    },
    /// The client gave up on it, in this state.
    Failed(String),
}

/// A client's job as asked after: where it stands, and the client's own
/// word for its state (such as `stalledUP` or `Downloading`), kept for the
/// record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStatus {
    /// Where it stands.
    pub phase: Phase,
    /// The client's word for it, at most 64 characters.
    pub state: String,
}

impl JobStatus {
    /// A status, the client's word cut to 64 characters.
    #[must_use]
    pub fn new(phase: Phase, state: &str) -> Self {
        Self {
            phase,
            state: state.chars().take(64).collect(),
        }
    }
}

/// Where a path the client reported lies beneath its download folder, as a
/// relative path to resolve under the watched root it is mounted at.
///
/// `None` for anything not strictly beneath `client_path`: another folder,
/// the folder itself (which would import everything in it), an empty or
/// relative path, or one with `.`, `..` or empty segments. The watched root
/// checks the result again against the real filesystem.
#[must_use]
pub fn relative_to(client_path: &str, content_path: &str) -> Option<String> {
    let base = client_path.trim_end_matches('/');
    if !client_path.starts_with('/') || !content_path.starts_with('/') {
        return None;
    }
    let rest = content_path.strip_prefix(base)?.strip_prefix('/')?;
    let rest = rest.trim_end_matches('/');
    let usable = !rest.is_empty()
        && rest
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
        && !rest.chars().any(char::is_control);
    usable.then(|| rest.to_owned())
}

/// A response body as text, read up to [`MAX_BODY_BYTES`].
///
/// # Errors
///
/// [`ClientError`] for a failed read, a body too large, or one not UTF-8.
pub async fn read_text(
    client: &'static str,
    response: reqwest::Response,
) -> Result<String, ClientError> {
    read_text_within(client, response, MAX_BODY_BYTES).await
}

/// A response body as text, read up to `max` bytes, for an answer known to
/// run larger than most (a client's whole history).
///
/// # Errors
///
/// As [`read_text`].
pub async fn read_text_within(
    client: &'static str,
    mut response: reqwest::Response,
    max: usize,
) -> Result<String, ClientError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ClientError::Unreachable {
            client,
            detail: error.without_url().to_string(),
        })?
    {
        if body.len() + chunk.len() > max {
            return Err(ClientError::bad(
                client,
                format!("the answer was larger than {max} bytes"),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| ClientError::bad(client, "the answer was not text"))
}

/// A `multipart/form-data` body: text `fields`, then one file part. Answers
/// the content type and the body.
#[must_use]
pub fn multipart(
    fields: &[(&str, &str)],
    file_field: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> (String, Vec<u8>) {
    // A boundary the file does not contain, checked rather than hoped for.
    let mut counter = 0_u32;
    let mut boundary = boundary_for(bytes, counter);
    while bytes
        .windows(boundary.len())
        .any(|w| w == boundary.as_bytes())
    {
        counter += 1;
        boundary = boundary_for(bytes, counter);
    }
    let mut body = Vec::with_capacity(bytes.len() + 1024);
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\ncontent-disposition: form-data; name=\"{file_field}\"; \
             filename=\"{filename}\"\r\ncontent-type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// A multipart boundary derived from the file and a counter.
fn boundary_for(bytes: &[u8], counter: u32) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::new()
        .chain_update(bytes)
        .chain_update(counter.to_le_bytes())
        .finalize();
    format!("tangible-{}", hex::encode(&digest[..12]))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_reported_path_maps_only_from_strictly_beneath_the_download_folder() {
        let base = "/downloads/tangible";
        assert_eq!(
            relative_to(base, "/downloads/tangible/disc.iso").as_deref(),
            Some("disc.iso")
        );
        assert_eq!(
            relative_to("/downloads/tangible/", "/downloads/tangible/Disc (CUE)/").as_deref(),
            Some("Disc (CUE)")
        );
        assert_eq!(relative_to("/", "/disc.iso").as_deref(), Some("disc.iso"));
        for outside in [
            "/downloads/tangible",
            "/downloads/tangible/",
            "/downloads/tangiblex/disc.iso",
            "/downloads/other/disc.iso",
            "/downloads/tangible/../etc/passwd",
            "/downloads/tangible/./disc.iso",
            "/downloads/tangible//disc.iso",
            "downloads/tangible/disc.iso",
            "",
            "/downloads/tangible/a\nb",
        ] {
            assert_eq!(relative_to(base, outside), None, "{outside:?}");
        }
        assert_eq!(relative_to("relative", "relative/x"), None);
    }

    #[test]
    fn a_multipart_body_holds_its_fields_and_file() {
        let file = b"<nzb>--tangible-not-a-boundary</nzb>";
        let (content_type, body) = multipart(
            &[("mode", "addfile"), ("cat", "tangible")],
            "name",
            "x.nzb",
            "application/x-nzb",
            file,
        );
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        assert!(
            !file
                .windows(boundary.len())
                .any(|w| w == boundary.as_bytes())
        );
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"mode\"\r\n\r\naddfile\r\n"));
        assert!(
            text.contains("name=\"name\"; filename=\"x.nzb\"\r\ncontent-type: application/x-nzb")
        );
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
        assert!(body.windows(file.len()).any(|w| w == file));
    }

    #[test]
    fn every_failure_names_its_client_and_has_a_code() {
        let error = ClientError::Auth {
            client: "SABnzbd",
            reason: "the API key is wrong",
        };
        assert_eq!(
            error.to_string(),
            "SABnzbd refused to sign in: the API key is wrong"
        );
        assert_eq!(error.code(), "INTEGRATION_AUTH_FAILED");
        assert_eq!(
            ClientError::bad("NZBGet", "x").to_string(),
            "NZBGet answered unexpectedly: x"
        );
    }
}
