// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A SABnzbd the operator already runs (docs/plans/acquisition.md A5,
//! ADR 0016).
//!
//! Asked for four things only: its version, its categories, to add an NZB
//! the operator supplied to Tangible's category with no script, and how one
//! job is doing. Never to search, run a script, or delete anything. Every
//! request is a POST with the API key in the body, so the key is never in a
//! URL, and goes through the outbound client under the integrations policy.
//!
//! Behaviour recorded from SABnzbd 4.5.3, downloading from a stand-in news
//! server, in fixtures/tool-output/sabnzbd:
//!
//! - Without the right key, every mode but `version` answers 403, `API Key
//!   Required` or `API Key Incorrect`. A host name not in its
//!   `host_whitelist` is refused with 403 `Hostname verification failed`.
//! - `addfile` answers `{"status": true, "nzo_ids": [...]}`, or `status`
//!   false for a file it cannot read. The same NZB added twice is two jobs.
//! - A category it does not have is quietly replaced by `*`, so the
//!   category is checked before anything is added.
//! - A job is in the queue while it downloads, then in history, `Completed`
//!   with `storage` naming the file or folder, or `Failed` with a
//!   `fail_message` and `storage` in the incomplete folder.

use std::fmt;

use serde::Deserialize;
use tangible_domain::SecretString;

use crate::download::{ClientError, JobStatus, Phase, multipart, read_text};
use crate::outbound::{OutboundClient, OutboundError, Url};

/// The client, in words, for errors.
const CLIENT: &str = "SABnzbd";

fn outbound(error: OutboundError) -> ClientError {
    ClientError::from_outbound(CLIENT, error)
}

/// A SABnzbd's API.
pub struct SabnzbdClient {
    http: OutboundClient,
    base: Url,
    api_key: SecretString,
}

impl fmt::Debug for SabnzbdClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SabnzbdClient")
            .field("base", &crate::outbound::redacted(&self.base))
            .finish_non_exhaustive()
    }
}

impl SabnzbdClient {
    /// A client for the SABnzbd at `base` (its web address, with any URL
    /// base it is served under).
    #[must_use]
    pub fn new(http: OutboundClient, mut base: Url, api_key: SecretString) -> Self {
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Self {
            http,
            base,
            api_key,
        }
    }

    async fn call(&self, content_type: String, body: Vec<u8>) -> Result<String, ClientError> {
        let url = self
            .base
            .join("api")
            .map_err(|_| ClientError::bad(CLIENT, "the address cannot be joined"))?;
        let request = self
            .http
            .request(reqwest::Method::POST, &url)
            .map_err(outbound)?
            .header("content-type", content_type)
            .body(body);
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status().as_u16();
        let text = read_text(CLIENT, response).await?;
        match status {
            200 => Ok(text),
            403 if text.contains("Hostname verification") => Err(ClientError::Auth {
                client: CLIENT,
                reason: "it does not accept this host name; use its IP address, or add the \
                         name to host_whitelist in SABnzbd's special settings",
            }),
            403 if text.contains("API Key") => Err(ClientError::Auth {
                client: CLIENT,
                reason: "the API key is wrong or missing",
            }),
            code => Err(ClientError::bad(CLIENT, format!("the API answered {code}"))),
        }
    }

    async fn mode(&self, mode: &str, extra: &[(&str, &str)]) -> Result<String, ClientError> {
        // Built before the await: the serializer is not `Send`.
        let body = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("apikey", self.api_key.expose())
                .append_pair("mode", mode)
                .append_pair("output", "json");
            for (name, value) in extra {
                form.append_pair(name, value);
            }
            form.finish().into_bytes()
        };
        self.call("application/x-www-form-urlencoded".to_owned(), body)
            .await
    }

    /// SABnzbd's version, such as `4.5.3`. It needs no key: ask for the
    /// categories as well to check that.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn version(&self) -> Result<String, ClientError> {
        #[derive(Deserialize)]
        struct Answer {
            version: String,
        }
        let text = self.mode("version", &[]).await?;
        let answer: Answer = serde_json::from_str(&text)
            .map_err(|_| ClientError::bad(CLIENT, "the version did not parse; is this SABnzbd?"))?;
        Ok(answer.version.chars().take(32).collect())
    }

    /// The categories it has.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn categories(&self) -> Result<Vec<String>, ClientError> {
        parse_categories(&self.mode("get_cats", &[]).await?)
    }

    /// Add an NZB to `category` as `filename`, with no script. Answers its
    /// job id, or `None` when SABnzbd could not read the file.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn add(
        &self,
        nzb: &[u8],
        filename: &str,
        category: &str,
    ) -> Result<Option<String>, ClientError> {
        let (content_type, body) = multipart(
            &[
                ("apikey", self.api_key.expose()),
                ("mode", "addfile"),
                ("output", "json"),
                ("cat", category),
                // Explicitly none, whatever the category says.
                ("script", "None"),
            ],
            "name",
            filename,
            "application/x-nzb",
            nzb,
        );
        parse_added(&self.call(content_type, body).await?)
    }

    /// Where job `nzo_id` stands, or `None` when SABnzbd has no such job.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn job(&self, nzo_id: &str) -> Result<Option<JobStatus>, ClientError> {
        if let Some(phase) =
            parse_queue(&self.mode("queue", &[("nzo_ids", nzo_id)]).await?, nzo_id)?
        {
            return Ok(Some(phase));
        }
        parse_history(&self.mode("history", &[("nzo_ids", nzo_id)]).await?, nzo_id)
    }
}

/// The categories in a `get_cats` answer.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_categories(text: &str) -> Result<Vec<String>, ClientError> {
    #[derive(Deserialize)]
    struct Answer {
        categories: Vec<String>,
    }
    serde_json::from_str::<Answer>(text)
        .map(|answer| answer.categories)
        .map_err(|_| ClientError::bad(CLIENT, "the category list did not parse"))
}

/// The job an `addfile` answer names, or `None` when SABnzbd declined.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_added(text: &str) -> Result<Option<String>, ClientError> {
    #[derive(Deserialize)]
    struct Answer {
        status: bool,
        #[serde(default)]
        nzo_ids: Vec<String>,
    }
    let answer: Answer = serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "adding the NZB answered something unreadable"))?;
    Ok(answer
        .status
        .then(|| answer.nzo_ids.into_iter().next())
        .flatten()
        .filter(|id| usable_id(id)))
}

/// A job id as SABnzbd makes them: `SABnzbd_nzo_` and a few letters.
fn usable_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Megabytes as SABnzbd writes them (`"0.29"`), as bytes.
fn megabytes(text: &str) -> Option<i64> {
    let value: f64 = text.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    // A rounded figure for a progress bar; the exact size comes from the
    // file itself.
    #[allow(clippy::cast_possible_truncation)]
    Some((value * 1_048_576.0).round() as i64)
}

/// Where job `nzo_id` stands in a `queue` answer, if it is there.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_queue(text: &str, nzo_id: &str) -> Result<Option<JobStatus>, ClientError> {
    #[derive(Deserialize)]
    struct Answer {
        queue: Queue,
    }
    #[derive(Deserialize)]
    struct Queue {
        #[serde(default)]
        slots: Vec<Slot>,
    }
    #[derive(Deserialize)]
    struct Slot {
        nzo_id: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        mb: String,
        #[serde(default)]
        mbleft: String,
    }
    let answer: Answer = serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "the queue did not parse"))?;
    Ok(answer
        .queue
        .slots
        .into_iter()
        .find(|slot| slot.nzo_id == nzo_id)
        .map(|slot| {
            let total = megabytes(&slot.mb).filter(|&total| total > 0);
            let left = megabytes(&slot.mbleft);
            let phase = Phase::Downloading {
                done: total
                    .zip(left)
                    .map(|(total, left)| (total - left).clamp(0, total)),
                total,
            };
            JobStatus::new(phase, &slot.status)
        }))
}

/// Where job `nzo_id` stands in a `history` answer, if it is there.
///
/// `Completed` is done, at `storage`; `Failed` failed, saying why; anything
/// else (verifying, repairing, unpacking, moving) is still on its way.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_history(text: &str, nzo_id: &str) -> Result<Option<JobStatus>, ClientError> {
    #[derive(Deserialize)]
    struct Answer {
        history: History,
    }
    #[derive(Deserialize)]
    struct History {
        #[serde(default)]
        slots: Vec<Slot>,
    }
    #[derive(Deserialize)]
    struct Slot {
        nzo_id: String,
        status: String,
        #[serde(default)]
        storage: Option<String>,
        #[serde(default)]
        bytes: Option<i64>,
        #[serde(default)]
        fail_message: Option<String>,
    }
    let answer: Answer = serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "the history did not parse"))?;
    Ok(answer
        .history
        .slots
        .into_iter()
        .find(|slot| slot.nzo_id == nzo_id)
        .map(|slot| {
            let phase = match slot.status.as_str() {
                "Completed" => match slot.storage.filter(|path| !path.is_empty()) {
                    Some(content_path) => Phase::Complete {
                        content_path,
                        size: slot.bytes.unwrap_or(0).max(0),
                    },
                    None => Phase::Failed("Completed, without saying where".to_owned()),
                },
                "Failed" => Phase::Failed(
                    slot.fail_message
                        .filter(|message| !message.is_empty())
                        .map_or_else(
                            || "Failed".to_owned(),
                            |message| format!("Failed: {message}"),
                        )
                        .chars()
                        .take(300)
                        .collect(),
                ),
                _ => Phase::Downloading {
                    done: slot.bytes,
                    total: slot.bytes,
                },
            };
            JobStatus::new(phase, &slot.status)
        }))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../../fixtures/tool-output/sabnzbd/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn a_recorded_add_names_its_job_and_a_refusal_names_none() {
        assert_eq!(
            parse_added(&fixture("addfile-4.5.3.json"))
                .unwrap()
                .as_deref(),
            Some("SABnzbd_nzo_tv81m40r")
        );
        assert_eq!(
            parse_added(r#"{"status":false,"nzo_ids":[]}"#).unwrap(),
            None
        );
        assert_eq!(
            parse_added(r#"{"status":true,"nzo_ids":["../../etc"]}"#).unwrap(),
            None,
            "an id that is not one is not followed"
        );
        assert!(parse_added("API Key Incorrect").is_err());
    }

    #[test]
    fn a_recorded_queue_reads_as_downloading() {
        let queue = fixture("queue-4.5.3.json");
        let status = parse_queue(&queue, "SABnzbd_nzo_tv81m40r")
            .unwrap()
            .unwrap();
        assert_eq!(status.state, "Queued");
        let Phase::Downloading { done, total } = status.phase else {
            panic!("downloading");
        };
        assert_eq!(total, Some(304_087));
        assert_eq!(done, Some(0));
        assert_eq!(parse_queue(&queue, "SABnzbd_nzo_other").unwrap(), None);
    }

    #[test]
    fn a_recorded_history_reads_as_complete_or_failed() {
        let history = fixture("history-4.5.3.json");
        let done = parse_history(&history, "SABnzbd_nzo_s_6yxmis")
            .unwrap()
            .unwrap();
        assert_eq!(done.state, "Completed");
        assert_eq!(
            done.phase,
            Phase::Complete {
                content_path: "/config/Downloads/complete/tangible/probe/probe.iso".to_owned(),
                size: 300_000,
            }
        );
        let failed = parse_history(&history, "SABnzbd_nzo_r_no3t4f")
            .unwrap()
            .unwrap();
        assert_eq!(failed.state, "Failed");
        let Phase::Failed(why) = failed.phase else {
            panic!("failed");
        };
        assert!(
            why.starts_with("Failed: Download failed - Not on your server"),
            "{why}"
        );
        assert_eq!(
            parse_history(&history, "SABnzbd_nzo_nonesuch").unwrap(),
            None
        );
    }

    #[test]
    fn a_job_still_post_processing_is_on_its_way() {
        let text = r#"{"history":{"slots":[{"nzo_id":"n","status":"Extracting","bytes":10}]}}"#;
        assert_eq!(
            parse_history(text, "n").unwrap().map(|status| status.phase),
            Some(Phase::Downloading {
                done: Some(10),
                total: Some(10)
            })
        );
        let empty = r#"{"history":{"slots":[{"nzo_id":"n","status":"Completed","storage":""}]}}"#;
        assert!(matches!(
            parse_history(empty, "n")
                .unwrap()
                .map(|status| status.phase),
            Some(Phase::Failed(_))
        ));
    }

    #[test]
    fn categories_and_megabytes_read() {
        assert_eq!(
            parse_categories(r#"{"categories":["*","tangible"]}"#).unwrap(),
            ["*", "tangible"]
        );
        assert_eq!(megabytes("0.29"), Some(304_087));
        assert_eq!(megabytes("-1"), None);
        assert_eq!(megabytes("NaN"), None);
        assert_eq!(megabytes(""), None);
    }
}
