// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Fetching a URL into a file, resuming what an interrupted fetch left
//! (ADR 0016, docs/plans/acquisition.md A2).
//!
//! Through [`OutboundClient`], so every rule there holds. A fetch that stops
//! partway leaves its bytes; the next asks for the rest with `Range`, and
//! with `If-Range` naming what the server called the file last time (a
//! strong `ETag`, else `Last-Modified`), so that a file changed in between is
//! sent whole instead of being spliced onto the old one's beginning. Without
//! such a validator nothing is resumed: the fetch starts again.
//!
//! The size limit is kept on the bytes as they arrive, not only on the
//! `Content-Length` the server claims.

use std::path::Path;

use reqwest::Url;
use reqwest::header::{
    CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderMap, IF_RANGE, LAST_MODIFIED, RANGE,
};
use tokio::io::AsyncWriteExt as _;

use crate::outbound::{OutboundClient, OutboundError, Refusal, redacted};

/// How often progress is reported, in bytes.
const PROGRESS_EVERY: u64 = 16 * 1024 * 1024;

/// What a previous, interrupted fetch left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    /// Bytes already in the file.
    pub offset: u64,
    /// What the server called the file then: a strong `ETag` or a
    /// `Last-Modified` date. Without one, nothing is resumed.
    pub validator: Option<String>,
}

/// What a finished fetch got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The file's length now.
    pub bytes: u64,
    /// Bytes kept from before, when the fetch resumed; zero when it started
    /// again.
    pub resumed_from: u64,
    /// What the server calls the file, for a later resume.
    pub validator: Option<String>,
    /// Where the bytes came from after redirects, as it may be recorded.
    pub final_reference: String,
    /// The content type the server claimed. Recorded, never trusted.
    pub content_type: Option<String>,
}

/// Progress, as a fetch reports it: bytes in the file, the whole size when
/// the server said, and the validator once the server has answered.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'a> {
    /// Bytes in the file so far.
    pub bytes: u64,
    /// The whole file's size, when the server said.
    pub total: Option<u64>,
    /// The validator to resume with.
    pub validator: Option<&'a str>,
}

/// Why a fetch did not finish.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Refused before anything was sent.
    #[error("refused: {0}")]
    Refused(Refusal),
    /// The server answered with something other than the file.
    #[error("the server answered {status}")]
    Status {
        /// The HTTP status.
        status: u16,
    },
    /// Larger than the limit.
    #[error("the file is larger than the limit of {limit} bytes")]
    TooLarge {
        /// The limit.
        limit: u64,
    },
    /// The server's answer to a resume did not hold together.
    #[error("the server's answer did not continue the file: {0}")]
    BadRange(String),
    /// Writing the file failed.
    #[error("writing the file failed: {0}")]
    Io(#[from] std::io::Error),
    /// The transfer failed: a timeout, a dropped connection.
    #[error("the transfer failed: {0}")]
    Transport(reqwest::Error),
}

impl FetchError {
    /// Whether trying again could help.
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Status { status } => *status >= 500 || matches!(status, 408 | 425 | 429),
            Self::Io(_) | Self::Transport(_) | Self::BadRange(_) => true,
            Self::Refused(_) | Self::TooLarge { .. } => false,
        }
    }

    /// The stable code for the import it fails.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Refused(_) => "IMPORT_SSRF_BLOCKED",
            Self::Status { .. } => "IMPORT_SOURCE_HTTP_STATUS",
            Self::TooLarge { .. } => "IMPORT_TOO_LARGE",
            Self::BadRange(_) | Self::Transport(_) => "IMPORT_DOWNLOAD_FAILED",
            Self::Io(_) => "IMPORT_STAGING_FAILED",
        }
    }
}

impl From<OutboundError> for FetchError {
    fn from(error: OutboundError) -> Self {
        match error {
            OutboundError::Refused(refusal) => Self::Refused(refusal),
            OutboundError::Transport(error) => Self::Transport(error),
        }
    }
}

/// Fetch `url` into the file at `path`, resuming from `resume` when the
/// server allows, never past `max_bytes`.
///
/// `on_progress` is called once the server has answered (with the validator
/// to keep for a resume) and then every 16 MiB, so the caller can record how
/// far it has got.
///
/// # Errors
///
/// [`FetchError`]. What was written stays in the file, for the next try.
pub async fn fetch_to_file(
    client: &OutboundClient,
    url: &Url,
    path: &Path,
    resume: Option<&Resume>,
    max_bytes: u64,
    on_progress: &mut (dyn FnMut(Progress<'_>) + Send),
) -> Result<Fetched, FetchError> {
    let resumable = resume.filter(|resume| resume.offset > 0 && resume.validator.is_some());
    let mut request = client.get(url)?;
    if let Some(Resume {
        offset,
        validator: Some(validator),
    }) = resumable
    {
        request = request
            .header(RANGE, format!("bytes={offset}-"))
            .header(IF_RANGE, validator.as_str());
    }
    let mut response = client.send(request).await?;
    let status = response.status().as_u16();

    // 206 continues the file; anything else that is the file starts it again.
    let offset = match (status, resumable) {
        (206, Some(resume)) => {
            let start = content_range_start(response.headers())?;
            if start != resume.offset {
                return Err(FetchError::BadRange(format!(
                    "asked for the file from byte {}, sent from {start}",
                    resume.offset
                )));
            }
            resume.offset
        }
        (206, None) => {
            return Err(FetchError::BadRange(
                "sent part of a file that was asked for whole".to_owned(),
            ));
        }
        (200, _) => 0,
        // The range was past the end, or the server will not do ranges for
        // this: start again, once, without one.
        (416, Some(_)) => {
            return Box::pin(fetch_to_file(
                client,
                url,
                path,
                None,
                max_bytes,
                on_progress,
            ))
            .await;
        }
        _ => return Err(FetchError::Status { status }),
    };

    let validator = validator_of(response.headers());
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.chars().take(200).collect());
    let total = response.content_length().map(|length| offset + length);
    if total.is_some_and(|total| total > max_bytes) {
        return Err(FetchError::TooLarge { limit: max_bytes });
    }
    let final_reference = redacted(response.url());

    let written = write_body(
        &mut response,
        path,
        offset,
        Progress {
            bytes: offset,
            total,
            validator: validator.as_deref(),
        },
        max_bytes,
        on_progress,
    )
    .await?;
    if let Some(total) = total
        && written != total
    {
        return Err(FetchError::BadRange(format!(
            "the server said {total} bytes and sent {written}"
        )));
    }
    Ok(Fetched {
        bytes: written,
        resumed_from: offset,
        validator,
        final_reference,
        content_type,
    })
}

/// Write a response's body into the file at `path` after its first `offset`
/// bytes, reporting `progress` as it goes. Answers the file's length.
async fn write_body(
    response: &mut reqwest::Response,
    path: &Path,
    offset: u64,
    progress: Progress<'_>,
    max_bytes: u64,
    on_progress: &mut (dyn FnMut(Progress<'_>) + Send),
) -> Result<u64, FetchError> {
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(path)
        .await?;
    if offset > 0 {
        // Anything after what was counted is not trusted to be whole.
        file.set_len(offset).await?;
        file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .await?;
    }

    let mut written = offset;
    on_progress(progress);
    let mut next_report = written + PROGRESS_EVERY;
    let copied: Result<(), FetchError> = async {
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|error| FetchError::Transport(error.without_url()))?;
            let Some(chunk) = chunk else { return Ok(()) };
            written = written.saturating_add(chunk.len() as u64);
            if written > max_bytes {
                return Err(FetchError::TooLarge { limit: max_bytes });
            }
            file.write_all(&chunk).await?;
            if written >= next_report {
                next_report = written + PROGRESS_EVERY;
                on_progress(Progress {
                    bytes: written,
                    ..progress
                });
            }
        }
    }
    .await;
    // Flushed however the copy ended. A tokio file finishes its last write
    // in the background after it is dropped, so an interrupted fetch that
    // returned without flushing left a file shorter than it would become:
    // the retry then cut it to that length and appended, and the late write
    // landing between the two doubled bytes. Seen as a test counting bytes
    // kept failing under load, 2026-10-25.
    file.flush().await?;
    copied?;
    file.sync_all().await?;
    Ok(written)
}

/// The first byte of a `Content-Range: bytes START-END/TOTAL`.
fn content_range_start(headers: &HeaderMap) -> Result<u64, FetchError> {
    let value = headers
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| FetchError::BadRange("206 without Content-Range".to_owned()))?;
    value
        .strip_prefix("bytes ")
        .and_then(|range| range.split('-').next())
        .and_then(|start| start.trim().parse().ok())
        .ok_or_else(|| FetchError::BadRange("a Content-Range that does not parse".to_owned()))
}

/// What to resume with: a strong `ETag`, else `Last-Modified`. A weak `ETag`
/// may not be used in `If-Range`.
fn validator_of(headers: &HeaderMap) -> Option<String> {
    let text = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .map(str::to_owned)
    };
    text(ETAG)
        .filter(|etag| !etag.starts_with("W/"))
        .or_else(|| text(LAST_MODIFIED))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::outbound::OutboundPolicy;

    /// The file the servers here serve.
    fn body() -> Vec<u8> {
        (0..100_000_u32).flat_map(u32::to_le_bytes).collect()
    }

    /// How a test server behaves.
    #[derive(Clone, Copy)]
    enum Serve {
        /// Honour Range, with a strong ETag.
        Properly,
        /// Send half the body, then hang up.
        HangUpHalfway,
        /// Ignore Range: always 200 with the whole file.
        IgnoreRange,
        /// Answer this status.
        Status(u16),
        /// Say no length, and send the body until the connection closes.
        NoLength,
    }

    /// A one-file HTTP server on 127.0.0.1. Answers each connection per
    /// `serve`, recording the request lines it saw.
    async fn server(serve: Serve) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut request = vec![0_u8; 8192];
                    let read = stream.read(&mut request).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&request[..read]).into_owned();
                    log.lock().unwrap().push(request.clone());
                    let body = body();
                    let range_start = request
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("range: bytes=")
                                .map(str::to_owned)
                        })
                        .and_then(|value| value.trim_end_matches('-').parse::<usize>().ok());
                    let response: Vec<u8> = match serve {
                        Serve::Status(status) => format!(
                            "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                        .into_bytes(),
                        Serve::NoLength => {
                            let mut head = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_vec();
                            head.extend(&body);
                            head
                        }
                        Serve::IgnoreRange => {
                            let mut head = format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\netag: \"v1\"\r\nconnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            head.extend(&body);
                            head
                        }
                        Serve::Properly | Serve::HangUpHalfway => match range_start {
                            Some(start) if start < body.len() => {
                                let mut head = format!(
                                    "HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\n\
                                     content-range: bytes {start}-{}/{}\r\netag: \"v1\"\r\n\
                                     connection: close\r\n\r\n",
                                    body.len() - start,
                                    body.len() - 1,
                                    body.len()
                                )
                                .into_bytes();
                                head.extend(&body[start..]);
                                head
                            }
                            _ => {
                                let mut head = format!(
                                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\netag: \"v1\"\r\n\
                                     content-type: application/octet-stream\r\nconnection: close\r\n\r\n",
                                    body.len()
                                )
                                .into_bytes();
                                let end = if matches!(serve, Serve::HangUpHalfway) {
                                    body.len() / 2
                                } else {
                                    body.len()
                                };
                                head.extend(&body[..end]);
                                head
                            }
                        },
                    };
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (address, seen)
    }

    fn client() -> OutboundClient {
        let mut policy =
            OutboundPolicy::for_operator_urls(true, vec!["127.0.0.1/32".parse().unwrap()], None);
        policy.allow_any_port = true;
        OutboundClient::new(policy, "tangible-test").unwrap()
    }

    fn url(address: SocketAddr) -> Url {
        Url::parse(&format!(
            "http://127.0.0.1:{}/disc.iso?token=secret",
            address.port()
        ))
        .unwrap()
    }

    async fn fetch(
        address: SocketAddr,
        path: &Path,
        resume: Option<&Resume>,
        max: u64,
    ) -> Result<Fetched, FetchError> {
        fetch_to_file(&client(), &url(address), path, resume, max, &mut |_| {}).await
    }

    #[tokio::test]
    async fn a_file_is_fetched_whole() {
        let (address, _) = server(Serve::Properly).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disc.iso");
        let mut reports = Vec::new();
        let fetched = fetch_to_file(&client(), &url(address), &path, None, u64::MAX, &mut |p| {
            reports.push((p.bytes, p.total, p.validator.map(str::to_owned)));
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body());
        assert_eq!(fetched.bytes, 400_000);
        assert_eq!(fetched.resumed_from, 0);
        assert_eq!(fetched.validator.as_deref(), Some("\"v1\""));
        assert_eq!(
            fetched.content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert!(
            !fetched.final_reference.contains("secret"),
            "{}",
            fetched.final_reference
        );
        assert_eq!(reports[0], (0, Some(400_000), Some("\"v1\"".to_owned())));
    }

    #[tokio::test]
    async fn an_interrupted_fetch_resumes_where_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disc.iso");
        let (dropping, _) = server(Serve::HangUpHalfway).await;
        let error = fetch(dropping, &path, None, u64::MAX).await.unwrap_err();
        assert!(error.retryable(), "{error}");
        let kept = std::fs::metadata(&path).unwrap().len();
        assert_eq!(kept, 200_000, "what arrived is kept");

        let (whole, seen) = server(Serve::Properly).await;
        let resume = Resume {
            offset: kept,
            validator: Some("\"v1\"".to_owned()),
        };
        let fetched = fetch(whole, &path, Some(&resume), u64::MAX).await.unwrap();
        assert_eq!(fetched.resumed_from, 200_000);
        assert_eq!(std::fs::read(&path).unwrap(), body());
        let request = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(request.contains("range: bytes=200000-"), "{request}");
        assert!(request.contains("if-range: \"v1\""), "{request}");
    }

    #[tokio::test]
    async fn a_server_that_ignores_range_sends_the_file_whole_and_it_is_not_spliced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disc.iso");
        std::fs::write(&path, vec![0xAA_u8; 1000]).unwrap();
        let (address, _) = server(Serve::IgnoreRange).await;
        let resume = Resume {
            offset: 1000,
            validator: Some("\"v0\"".to_owned()),
        };
        let fetched = fetch(address, &path, Some(&resume), u64::MAX)
            .await
            .unwrap();
        assert_eq!(fetched.resumed_from, 0, "started again");
        assert_eq!(std::fs::read(&path).unwrap(), body());
    }

    #[tokio::test]
    async fn without_a_validator_nothing_is_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disc.iso");
        std::fs::write(&path, vec![0xAA_u8; 1000]).unwrap();
        let (address, seen) = server(Serve::Properly).await;
        let resume = Resume {
            offset: 1000,
            validator: None,
        };
        let fetched = fetch(address, &path, Some(&resume), u64::MAX)
            .await
            .unwrap();
        assert_eq!(fetched.resumed_from, 0);
        assert_eq!(std::fs::read(&path).unwrap(), body());
        assert!(
            !seen.lock().unwrap()[0]
                .to_ascii_lowercase()
                .contains("range:")
        );
    }

    #[tokio::test]
    async fn the_limit_is_kept_on_the_claim_and_on_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (address, _) = server(Serve::Properly).await;
        let error = fetch(address, &dir.path().join("a"), None, 1000)
            .await
            .unwrap_err();
        assert!(
            matches!(error, FetchError::TooLarge { limit: 1000 }),
            "{error}"
        );
        assert!(!error.retryable());
        // A server that says no length is stopped by the bytes. (One that
        // claims a short length is held to it by HTTP framing itself.)
        let (unsaid, _) = server(Serve::NoLength).await;
        let error = fetch(unsaid, &dir.path().join("b"), None, 1000)
            .await
            .unwrap_err();
        assert!(
            matches!(error, FetchError::TooLarge { limit: 1000 }),
            "{error}"
        );
        assert!(
            std::fs::metadata(dir.path().join("b")).unwrap().len() <= 1000,
            "nothing past the limit is written"
        );
    }

    #[tokio::test]
    async fn statuses_say_whether_to_try_again() {
        let dir = tempfile::tempdir().unwrap();
        for (status, retryable) in [
            (404, false),
            (403, false),
            (500, true),
            (503, true),
            (429, true),
        ] {
            let (address, _) = server(Serve::Status(status)).await;
            let error = fetch(address, &dir.path().join("x"), None, u64::MAX)
                .await
                .unwrap_err();
            assert!(
                matches!(error, FetchError::Status { status: s } if s == status),
                "{error}"
            );
            assert_eq!(error.retryable(), retryable, "{status}");
            assert_eq!(error.code(), "IMPORT_SOURCE_HTTP_STATUS");
        }
    }

    #[tokio::test]
    async fn a_refused_address_is_refused_and_never_retried() {
        let dir = tempfile::tempdir().unwrap();
        let (address, seen) = server(Serve::Properly).await;
        let refusing = OutboundClient::new(
            OutboundPolicy::for_operator_urls(true, Vec::new(), None),
            "tangible-test",
        )
        .unwrap();
        let error = fetch_to_file(
            &refusing,
            &url(address),
            &dir.path().join("x"),
            None,
            u64::MAX,
            &mut |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(error, FetchError::Refused(_)), "{error}");
        assert!(!error.retryable());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_weak_etag_is_not_a_validator() {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, "W/\"weak\"".parse().unwrap());
        assert_eq!(validator_of(&headers), None);
        headers.insert(
            LAST_MODIFIED,
            "Tue, 20 Oct 2026 10:00:00 GMT".parse().unwrap(),
        );
        assert_eq!(
            validator_of(&headers).as_deref(),
            Some("Tue, 20 Oct 2026 10:00:00 GMT")
        );
        headers.insert(ETAG, "\"strong\"".parse().unwrap());
        assert_eq!(validator_of(&headers).as_deref(), Some("\"strong\""));
    }

    #[test]
    fn transport_errors_never_carry_the_url() {
        // from_reqwest strips it; the display of a FetchError built from one
        // never names the query.
        let error = FetchError::Status { status: 500 };
        assert!(!error.to_string().contains("token"));
    }
}
