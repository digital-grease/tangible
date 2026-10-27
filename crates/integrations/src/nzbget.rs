// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! An NZBGet the operator already runs (docs/plans/acquisition.md A5,
//! ADR 0016).
//!
//! Asked for four things, through its JSON-RPC API: its version, its
//! categories, to append an NZB the operator supplied to Tangible's category
//! with no post-processing parameters, and how one job is doing. And a fifth
//! only where an administrator set the integration to remove after import,
//! once the library's copy is checked and Tangible has deleted the
//! download's files itself: to hide the job in history
//! (docs/plans/source-retention.md, ADR 0017). Never to search, run a
//! script, or delete files. HTTP basic authentication, the
//! header marked sensitive; every request through the outbound client under
//! the integrations policy.
//!
//! Behaviour recorded from NZBGet 25.3, downloading from a stand-in news
//! server, in fixtures/tool-output/nzbget:
//!
//! - A missing or wrong password answers 401 with an empty body; an unknown
//!   method an `error` object.
//! - `append` answers a positive NZBID for anything: a file that is not an
//!   NZB fails later, in history (`FAILURE/SCAN`), and a category NZBGet
//!   does not have is taken as given and filed under that name. So the
//!   category is checked before anything is appended.
//! - A job is in `listgroups` while it downloads and post-processes, then in
//!   `history`: `SUCCESS/...` with `DestDir` (or `FinalDir`, when a script
//!   moved it) naming its folder, or `FAILURE/...`, `WARNING/...`,
//!   `DELETED/...`.
//! - Sizes are `...SizeLo` and `...SizeHi`, 32-bit halves of a byte count.
//! - `editqueue("HistoryDelete", "", [id])` hides an entry (still listed by
//!   `history(true)`), `HistoryFinalDelete` drops it; each answers `true`,
//!   or `false` for an id it does not have, and neither touches `DestDir`
//!   (checked 2026-10-27).
//! - `config` answers every setting, NZBGet's own passwords among them; only
//!   the category names are read from it, and nothing else is kept.

use std::fmt;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tangible_domain::SecretString;

use crate::download::{ClientError, JobStatus, MAX_BODY_BYTES, Phase, read_text_within};
use crate::outbound::{OutboundClient, OutboundError, Url};

/// The client, in words, for errors.
const CLIENT: &str = "NZBGet";

/// The most of a history answer read. NZBGet cannot be asked for one job's
/// history, only all of it, at about a kilobyte an entry.
const MAX_HISTORY_BYTES: usize = 32 * 1024 * 1024;

fn outbound(error: OutboundError) -> ClientError {
    ClientError::from_outbound(CLIENT, error)
}

/// An NZBGet's JSON-RPC API.
pub struct NzbgetClient {
    http: OutboundClient,
    endpoint: Url,
    username: String,
    password: SecretString,
}

impl fmt::Debug for NzbgetClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NzbgetClient")
            .field("endpoint", &crate::outbound::redacted(&self.endpoint))
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl NzbgetClient {
    /// A client for the NZBGet at `base` (its web address).
    ///
    /// # Errors
    ///
    /// [`ClientError::BadResponse`] when the endpoint cannot be formed.
    pub fn new(
        http: OutboundClient,
        mut base: Url,
        username: &str,
        password: SecretString,
    ) -> Result<Self, ClientError> {
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let endpoint = base
            .join("jsonrpc")
            .map_err(|_| ClientError::bad(CLIENT, "the address cannot be joined"))?;
        Ok(Self {
            http,
            endpoint,
            username: username.to_owned(),
            password,
        })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
        max: usize,
    ) -> Result<T, ClientError> {
        let body = serde_json::json!({ "method": method, "params": params, "id": 1 }).to_string();
        let request = self
            .http
            .request(reqwest::Method::POST, &self.endpoint)
            .map_err(outbound)?
            .basic_auth(&self.username, Some(self.password.expose()))
            .header("content-type", "application/json")
            .body(body);
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status().as_u16();
        if status == 401 {
            return Err(ClientError::Auth {
                client: CLIENT,
                reason: "the user name or password is wrong, or the user may not do this \
                         (a restricted or add-only user)",
            });
        }
        let text = read_text_within(CLIENT, response, max).await?;
        if status != 200 {
            return Err(ClientError::bad(
                CLIENT,
                format!("the API answered {status}"),
            ));
        }
        parse_reply(&text)
    }

    /// NZBGet's version, such as `25.3`.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn version(&self) -> Result<String, ClientError> {
        let version: String = self
            .call("version", serde_json::json!([]), MAX_BODY_BYTES)
            .await?;
        Ok(version.chars().take(32).collect())
    }

    /// The categories it has.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn categories(&self) -> Result<Vec<String>, ClientError> {
        let options: Vec<ConfigOption> = self
            .call("config", serde_json::json!([]), MAX_BODY_BYTES)
            .await?;
        Ok(category_names(options))
    }

    /// Append an NZB to `category` as `filename`, not paused, with no
    /// post-processing parameters and NZBGet's duplicate check off: Tangible
    /// decides what is a repeat. Answers its NZBID, or `None` when NZBGet
    /// refused it.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn append(
        &self,
        nzb: &[u8],
        filename: &str,
        category: &str,
    ) -> Result<Option<i64>, ClientError> {
        use base64::Engine as _;
        let content = base64::engine::general_purpose::STANDARD.encode(nzb);
        let id: i64 = self
            .call(
                "append",
                serde_json::json!([
                    filename,
                    content,
                    category,
                    0,
                    false,
                    false,
                    "",
                    0,
                    "FORCE",
                    []
                ]),
                MAX_BODY_BYTES,
            )
            .await?;
        Ok((id > 0).then_some(id))
    }

    /// Hide job `nzbid` in history: the gentler of NZBGet's two deletes,
    /// so it is still listed among hidden entries. Its files are not
    /// touched. Answers whether NZBGet had it.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn forget(&self, nzbid: i64) -> Result<bool, ClientError> {
        self.call(
            "editqueue",
            serde_json::json!(["HistoryDelete", "", [nzbid]]),
            MAX_BODY_BYTES,
        )
        .await
    }

    /// Where job `nzbid` stands, or `None` when NZBGet has no such job.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn job(&self, nzbid: i64) -> Result<Option<JobStatus>, ClientError> {
        let groups: Vec<Group> = self
            .call("listgroups", serde_json::json!([0]), MAX_BODY_BYTES)
            .await?;
        if let Some(status) = queued(groups, nzbid) {
            return Ok(Some(status));
        }
        let history: Vec<HistoryItem> = self
            .call("history", serde_json::json!([false]), MAX_HISTORY_BYTES)
            .await?;
        Ok(finished(history, nzbid))
    }
}

/// The `result` of a JSON-RPC reply, or its `error` as a failure.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for an error reply, or one that does not
/// parse.
pub fn parse_reply<T: DeserializeOwned>(text: &str) -> Result<T, ClientError> {
    #[derive(Deserialize)]
    struct Reply<T> {
        result: Option<T>,
        error: Option<RpcError>,
    }
    #[derive(Deserialize)]
    struct RpcError {
        #[serde(default)]
        message: String,
    }
    let reply: Reply<T> = serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "the reply did not parse; is this NZBGet?"))?;
    if let Some(error) = reply.error {
        return Err(ClientError::bad(
            CLIENT,
            format!(
                "it refused: {}",
                error.message.chars().take(200).collect::<String>()
            ),
        ));
    }
    reply
        .result
        .ok_or_else(|| ClientError::bad(CLIENT, "the reply had no result"))
}

/// One setting from `config`. Only the name and value are read, and only
/// category names are kept.
#[derive(Deserialize)]
pub struct ConfigOption {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Value", default)]
    value: String,
}

/// The category names among NZBGet's settings (`Category1.Name` and so on).
#[must_use]
pub fn category_names(options: Vec<ConfigOption>) -> Vec<String> {
    options
        .into_iter()
        .filter(|option| {
            option
                .name
                .strip_prefix("Category")
                .and_then(|rest| rest.strip_suffix(".Name"))
                .is_some_and(|number| {
                    !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                })
        })
        .map(|option| option.value)
        .filter(|name| !name.is_empty())
        .collect()
}

/// A byte count NZBGet gives as two 32-bit halves.
fn bytes(lo: u32, hi: u32) -> i64 {
    i64::from(hi) << 32 | i64::from(lo)
}

/// A job in the queue, as much as Tangible reads.
#[derive(Deserialize)]
pub struct Group {
    #[serde(rename = "NZBID")]
    nzbid: i64,
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "FileSizeLo", default)]
    file_size_lo: u32,
    #[serde(rename = "FileSizeHi", default)]
    file_size_hi: u32,
    #[serde(rename = "RemainingSizeLo", default)]
    remaining_size_lo: u32,
    #[serde(rename = "RemainingSizeHi", default)]
    remaining_size_hi: u32,
}

/// Where job `nzbid` stands in the queue, if it is there: downloading or
/// post-processing, both still on their way.
#[must_use]
pub fn queued(groups: Vec<Group>, nzbid: i64) -> Option<JobStatus> {
    groups
        .into_iter()
        .find(|group| group.nzbid == nzbid)
        .map(|group| {
            let total = bytes(group.file_size_lo, group.file_size_hi);
            let left = bytes(group.remaining_size_lo, group.remaining_size_hi);
            let total = (total > 0).then_some(total);
            JobStatus::new(
                Phase::Downloading {
                    done: total.map(|total| (total - left).clamp(0, total)),
                    total,
                },
                &group.status,
            )
        })
}

/// A job in history, as much as Tangible reads.
#[derive(Deserialize)]
pub struct HistoryItem {
    #[serde(rename = "NZBID")]
    nzbid: i64,
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "DestDir", default)]
    dest_dir: String,
    #[serde(rename = "FinalDir", default)]
    final_dir: String,
    #[serde(rename = "FileSizeLo", default)]
    file_size_lo: u32,
    #[serde(rename = "FileSizeHi", default)]
    file_size_hi: u32,
}

/// Where job `nzbid` stands in history, if it is there. `SUCCESS/...` is
/// done, in `FinalDir` when a script moved it, else `DestDir`; anything else
/// (`FAILURE`, `WARNING`, `DELETED`) failed, in NZBGet's words.
#[must_use]
pub fn finished(history: Vec<HistoryItem>, nzbid: i64) -> Option<JobStatus> {
    history
        .into_iter()
        .find(|item| item.nzbid == nzbid)
        .map(|item| {
            let phase = if item.status.starts_with("SUCCESS/") {
                let folder = if item.final_dir.is_empty() {
                    item.dest_dir
                } else {
                    item.final_dir
                };
                if folder.is_empty() {
                    Phase::Failed(format!("{}, without saying where", item.status))
                } else {
                    Phase::Complete {
                        content_path: folder,
                        size: bytes(item.file_size_lo, item.file_size_hi),
                    }
                }
            } else {
                Phase::Failed(item.status.chars().take(64).collect())
            };
            JobStatus::new(phase, &item.status)
        })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../../fixtures/tool-output/nzbget/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn a_recorded_forget_says_whether_nzbget_had_the_job() {
        let had: bool = parse_reply(&fixture("editqueue-history-delete-25.3.json")).unwrap();
        assert!(had);
        let had: bool =
            parse_reply(&fixture("editqueue-history-delete-unknown-25.3.json")).unwrap();
        assert!(!had);
    }

    #[test]
    fn a_recorded_version_reads() {
        let version: String = parse_reply(&fixture("version-25.3.json")).unwrap();
        assert_eq!(version, "25.3");
    }

    #[test]
    fn an_error_reply_is_a_failure_saying_why() {
        let error = parse_reply::<String>(
            r#"{"version":"1.1","id":1,"error":{"name":"JSONRPCError","code":1,"message":"Invalid procedure"}}"#,
        )
        .expect_err("refused");
        assert!(error.to_string().contains("Invalid procedure"), "{error}");
        assert!(parse_reply::<String>("").is_err());
        assert!(parse_reply::<String>(r#"{"version":"1.1","id":1}"#).is_err());
    }

    #[test]
    fn a_recorded_history_reads_as_complete_or_failed() {
        let history: Vec<HistoryItem> = parse_reply(&fixture("history-25.3.json")).unwrap();
        let done = finished(history, 1).unwrap();
        assert_eq!(done.state, "SUCCESS/HEALTH");
        assert_eq!(
            done.phase,
            Phase::Complete {
                content_path: "/downloads/completed/tangible/probe".to_owned(),
                size: 300_000,
            }
        );
        for (nzbid, status) in [(2, "DELETED/COPY"), (3, "FAILURE/SCAN")] {
            let history: Vec<HistoryItem> = parse_reply(&fixture("history-25.3.json")).unwrap();
            assert_eq!(
                finished(history, nzbid).unwrap().phase,
                Phase::Failed(status.to_owned())
            );
        }
        let history: Vec<HistoryItem> = parse_reply(&fixture("history-25.3.json")).unwrap();
        assert!(finished(history, 99).is_none());
    }

    #[test]
    fn a_recorded_queue_reads_as_downloading() {
        let groups: Vec<Group> = parse_reply(&fixture("listgroups-25.3.json")).unwrap();
        let status = queued(groups, 4).unwrap();
        assert_eq!(status.state, "PAUSED");
        assert!(matches!(status.phase, Phase::Downloading { .. }));
        let groups: Vec<Group> = parse_reply(&fixture("listgroups-25.3.json")).unwrap();
        assert!(queued(groups, 1).is_none());
    }

    #[test]
    fn a_script_s_folder_wins_and_sizes_join_their_halves() {
        let item = |final_dir: &str| HistoryItem {
            nzbid: 7,
            status: "SUCCESS/UNPACK".to_owned(),
            dest_dir: "/dl/completed/tangible/x".to_owned(),
            final_dir: final_dir.to_owned(),
            file_size_lo: 5,
            file_size_hi: 1,
        };
        assert_eq!(
            finished(vec![item("/dl/moved/x")], 7).unwrap().phase,
            Phase::Complete {
                content_path: "/dl/moved/x".to_owned(),
                size: (1_i64 << 32) + 5,
            }
        );
        assert!(matches!(
            finished(vec![item("")], 7).unwrap().phase,
            Phase::Complete { content_path, .. } if content_path == "/dl/completed/tangible/x"
        ));
    }

    #[test]
    fn only_category_names_are_kept_from_the_config() {
        let options: Vec<ConfigOption> = serde_json::from_str(
            r#"[{"Name":"ControlPassword","Value":"secret"},
                {"Name":"Category1.Name","Value":"Movies"},
                {"Name":"Category1.DestDir","Value":"/x"},
                {"Name":"Category12.Name","Value":"tangible"},
                {"Name":"Category.Name","Value":"odd"},
                {"Name":"Category3.Name","Value":""}]"#,
        )
        .unwrap();
        assert_eq!(category_names(options), ["Movies", "tangible"]);
    }
}
