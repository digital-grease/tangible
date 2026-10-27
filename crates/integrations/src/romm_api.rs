// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The API of the RomM the export writes into (docs/plans/romm-api.md M1,
//! INTEGRATIONS.md 2.2).
//!
//! Asked to read, and to queue a quick scan of named platform folders: its
//! version, which of the operations Tangible uses its own OpenAPI document
//! has, its platforms, its ROMs, and how a scan it was asked for is doing.
//! Never to change, rename, delete or upload anything. Every request but the
//! version and the document carries a client token (RomM 5.0 and later) as
//! a bearer token, marked sensitive, and goes through the outbound client
//! under the integrations policy.
//!
//! Behaviour recorded from RomM 5.4.1, in fixtures/tool-output/romm:
//!
//! - `/api/heartbeat` and `/openapi.json` answer without a token. Every
//!   other route answers 401 `Not authenticated` without a token that opens,
//!   and 403 `Forbidden` to a token without the route's scope.
//! - A quick scan of a platform folder makes a new folder a ROM, keeps the
//!   ROM's id when the folder is renamed, and marks it `missing_from_fs`
//!   when the folder goes; a folder put back is the same ROM again.
//! - `POST /api/tasks/scan`, which a token may call, is new in 5.4.0; before
//!   it a scan needed a browser session. A slug it has no folder for is
//!   queued and scans nothing.
//! - `search_term` is fuzzy, so a ROM found by it is matched exactly by its
//!   platform folder and `fs_name` before it is believed.
//! - Every file in a game's folder is one of the game's files, the export's
//!   marker (`.tangible-export.json`) too, unless RomM's configuration names
//!   it in `exclude.roms.multi_file.parts.names`. Counted, it makes a
//!   one-image game a several-file one and goes out with every download.
//!   `/api/config`, without a token, lists what is excluded.
//!
//! RomM adds fields often, so answers parse into types that name only what
//! Tangible reads.

use std::fmt;

use serde::Deserialize;
use tangible_domain::SecretString;

use crate::download::{ClientError, read_text};
use crate::outbound::{OutboundClient, OutboundError, Url};

/// The client, in words, for errors.
const CLIENT: &str = "RomM";

/// Most ROMs asked for in one page.
pub const MAX_PAGE: u32 = 100;

fn outbound(error: OutboundError) -> ClientError {
    ClientError::from_outbound(CLIENT, error)
}

/// A scope a RomM client token carries, of the three Tangible asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Its platforms.
    PlatformsRead,
    /// Its ROMs and their files.
    RomsRead,
    /// Running tasks, of which Tangible runs only the scan.
    TasksRun,
}

impl Scope {
    /// Every scope Tangible asks for.
    pub const ALL: [Self; 3] = [Self::PlatformsRead, Self::RomsRead, Self::TasksRun];

    /// RomM's name for it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlatformsRead => "platforms.read",
            Self::RomsRead => "roms.read",
            Self::TasksRun => "tasks.run",
        }
    }

    /// A request that needs only this scope and changes nothing.
    const fn proof(self) -> (&'static str, &'static [(&'static str, &'static str)]) {
        match self {
            Self::PlatformsRead => ("api/platforms", &[]),
            Self::RomsRead => ("api/roms", &[("limit", "1")]),
            // The task definitions, a few kilobytes, rather than the status
            // list, which grows with every task run.
            Self::TasksRun => ("api/tasks", &[]),
        }
    }
}

/// An operation Tangible uses, as RomM's OpenAPI document must list it.
struct Operation {
    method: &'static str,
    path: &'static str,
    parameters: &'static [&'static str],
}

const fn operation(
    method: &'static str,
    path: &'static str,
    parameters: &'static [&'static str],
) -> Operation {
    Operation {
        method,
        path,
        parameters,
    }
}

/// Client tokens, which Tangible signs in with.
const CLIENT_TOKENS: &[Operation] = &[operation("get", "/api/client-tokens", &[])];

/// Finding an exported game: platforms, then ROMs by platform and name.
const LOOKUP: &[Operation] = &[
    operation("get", "/api/platforms", &[]),
    operation(
        "get",
        "/api/roms",
        &[
            "platform_ids",
            "search_term",
            "limit",
            "offset",
            "with_files",
        ],
    ),
    operation("get", "/api/roms/{id}", &["id"]),
];

/// The platforms RomM knows, whether or not it has games for them.
const KNOWN_PLATFORMS: &[Operation] = &[operation("get", "/api/platforms/supported", &[])];

/// Asking for a scan, and following it.
const SCAN: &[Operation] = &[
    operation("post", "/api/tasks/scan", &[]),
    operation("get", "/api/tasks/{task_id}", &["task_id"]),
];

/// Which of the operations Tangible uses an instance has, from its own
/// OpenAPI document. Each is found or not independently of the others.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Client tokens: RomM 5.0 and later.
    pub client_tokens: bool,
    /// Its platforms, and its ROMs by platform and name, with their files.
    pub lookup: bool,
    /// The platforms it knows, for flagging a slug it would not recognise.
    pub known_platforms: bool,
    /// A scan a token may ask for, and following it: RomM 5.4 and later.
    pub scan: bool,
}

/// Which operations an OpenAPI document lists.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for something that is not an OpenAPI 3
/// document.
pub fn capabilities(document: &str) -> Result<Capabilities, ClientError> {
    let document: serde_json::Value = serde_json::from_str(document)
        .map_err(|_| ClientError::bad(CLIENT, "its API document is not JSON"))?;
    let is_openapi = document
        .get("openapi")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|version| version.starts_with("3."));
    let Some(paths) = document.get("paths").filter(|_| is_openapi) else {
        return Err(ClientError::bad(
            CLIENT,
            "its API document is not an OpenAPI 3 document",
        ));
    };
    let has = |operations: &[Operation]| {
        operations.iter().all(|wanted| {
            let Some(found) = paths
                .get(wanted.path)
                .and_then(|path| path.get(wanted.method))
            else {
                return false;
            };
            let names: Vec<&str> = found
                .get("parameters")
                .and_then(serde_json::Value::as_array)
                .map(|parameters| {
                    parameters
                        .iter()
                        .filter_map(|parameter| parameter.get("name")?.as_str())
                        .collect()
                })
                .unwrap_or_default();
            wanted.parameters.iter().all(|name| names.contains(name))
        })
    };
    Ok(Capabilities {
        client_tokens: has(CLIENT_TOKENS),
        lookup: has(LOOKUP),
        known_platforms: has(KNOWN_PLATFORMS),
        scan: has(SCAN),
    })
}

/// What trying an instance found: its version, what it offers, which
/// scopes the token carries, and whether it leaves the export's marker out
/// of a game's files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Its version, such as `5.4.1`.
    pub version: String,
    /// What its document lists.
    pub capabilities: Capabilities,
    /// Each scope Tangible asks for, and whether the token carries it.
    pub scopes: Vec<(Scope, bool)>,
    /// Whether its configuration excludes the export's marker from a game's
    /// files; `None` when its configuration could not be read.
    pub ignores_marker: Option<bool>,
}

impl Probe {
    /// Whether the token carries `scope`.
    #[must_use]
    pub fn granted(&self, scope: Scope) -> bool {
        self.scopes
            .iter()
            .any(|(granted, yes)| *granted == scope && *yes)
    }

    /// Whether exported games can be found in it.
    #[must_use]
    pub fn can_link(&self) -> bool {
        self.capabilities.client_tokens
            && self.capabilities.lookup
            && self.granted(Scope::PlatformsRead)
            && self.granted(Scope::RomsRead)
    }

    /// Whether it can be asked to scan.
    #[must_use]
    pub fn can_scan(&self) -> bool {
        self.can_link() && self.capabilities.scan && self.granted(Scope::TasksRun)
    }
}

/// A platform RomM has games for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Platform {
    /// RomM's id for it.
    pub id: i64,
    /// Its slug, such as `psx`.
    pub slug: String,
    /// Its folder under `roms`, usually the slug.
    pub fs_slug: String,
}

/// One of a ROM's files, as RomM listed it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RomFile {
    /// Its name.
    pub file_name: String,
    /// The folder it is in, relative to RomM's library, such as
    /// `roms/psx/Game (USA)`.
    pub file_path: String,
    /// Its size.
    pub file_size_bytes: i64,
}

/// A ROM: for an exported game, its folder.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Rom {
    /// RomM's id for it, which its page is named by.
    pub id: i64,
    /// The platform's id.
    pub platform_id: i64,
    /// The platform's folder.
    pub platform_fs_slug: String,
    /// Its file or folder name.
    pub fs_name: String,
    /// The folder that holds it, relative to RomM's library, such as
    /// `roms/psx`.
    pub fs_path: String,
    /// Whether its last scan found it gone.
    #[serde(default)]
    pub missing_from_fs: bool,
    /// Its files, when they were asked for.
    #[serde(default)]
    pub files: Vec<RomFile>,
}

/// A page of ROMs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RomPage {
    /// The ROMs on it.
    pub items: Vec<Rom>,
    /// How many match in all, when RomM counted.
    #[serde(default)]
    pub total: Option<i64>,
}

/// Where a task stands, from RomM's word for it (RQ's job states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPhase {
    /// Not started: created, queued, scheduled or deferred.
    Waiting,
    /// Running.
    Running,
    /// Done.
    Finished,
    /// It failed.
    Failed,
    /// Stopped or canceled in RomM.
    Canceled,
}

impl TaskPhase {
    fn from_word(word: &str) -> Self {
        match word {
            "started" => Self::Running,
            "finished" => Self::Finished,
            "failed" => Self::Failed,
            "stopped" | "canceled" => Self::Canceled,
            _ => Self::Waiting,
        }
    }

    /// Whether it has ended, one way or another.
    #[must_use]
    pub const fn ended(self) -> bool {
        matches!(self, Self::Finished | Self::Failed | Self::Canceled)
    }
}

/// What a scan counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ScanStats {
    /// ROMs in the platforms scanned.
    pub total_roms: i64,
    /// ROMs it looked at.
    pub scanned_roms: i64,
    /// ROMs it made.
    pub new_roms: i64,
    /// ROMs it changed.
    pub updated_roms: i64,
}

/// A task RomM was asked to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// RomM's id for it.
    pub id: String,
    /// Where it stands.
    pub phase: TaskPhase,
    /// RomM's word for that, at most 32 characters.
    pub state: String,
    /// What it counted, for a scan that has started.
    pub stats: Option<ScanStats>,
}

/// RomM's version, from a heartbeat.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_version(text: &str) -> Result<String, ClientError> {
    #[derive(Deserialize)]
    struct System {
        #[serde(rename = "VERSION")]
        version: String,
    }
    #[derive(Deserialize)]
    struct Heartbeat {
        #[serde(rename = "SYSTEM")]
        system: System,
    }
    serde_json::from_str::<Heartbeat>(text)
        .map(|heartbeat| heartbeat.system.version.chars().take(32).collect())
        .map_err(|_| ClientError::bad(CLIENT, "the heartbeat did not parse; is this RomM?"))
}

/// Whether a configuration (`/api/config`) leaves the export's marker out
/// of a game's files: whether `EXCLUDED_MULTI_PARTS_FILES` names it. RomM
/// compares names regardless of case.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_ignores_marker(text: &str) -> Result<bool, ClientError> {
    #[derive(Deserialize)]
    struct Config {
        #[serde(rename = "EXCLUDED_MULTI_PARTS_FILES")]
        excluded: Vec<String>,
    }
    serde_json::from_str::<Config>(text)
        .map(|config| {
            config
                .excluded
                .iter()
                .any(|name| name.trim().eq_ignore_ascii_case(crate::romm::MARKER))
        })
        .map_err(|_| ClientError::bad(CLIENT, "its configuration did not parse"))
}

/// The platforms in a platform list.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_platforms(text: &str) -> Result<Vec<Platform>, ClientError> {
    serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "the platform list did not parse"))
}

/// The slugs in a list of the platforms RomM knows.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_known_platforms(text: &str) -> Result<Vec<String>, ClientError> {
    #[derive(Deserialize)]
    struct Known {
        slug: String,
    }
    serde_json::from_str::<Vec<Known>>(text)
        .map(|known| known.into_iter().map(|platform| platform.slug).collect())
        .map_err(|_| ClientError::bad(CLIENT, "the list of platforms it knows did not parse"))
}

/// A page of ROMs.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_rom_page(text: &str) -> Result<RomPage, ClientError> {
    serde_json::from_str(text).map_err(|_| ClientError::bad(CLIENT, "the ROM list did not parse"))
}

/// One ROM.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_rom(text: &str) -> Result<Rom, ClientError> {
    serde_json::from_str(text).map_err(|_| ClientError::bad(CLIENT, "the ROM did not parse"))
}

/// A task, as queued or as asked after.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for one that does not parse.
pub fn parse_task(text: &str) -> Result<Task, ClientError> {
    #[derive(Deserialize)]
    struct Meta {
        #[serde(default)]
        scan_stats: Option<ScanStats>,
    }
    #[derive(Deserialize)]
    struct Answer {
        task_id: String,
        status: String,
        #[serde(default)]
        meta: Option<Meta>,
    }
    let answer: Answer = serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "the task did not parse"))?;
    if !is_task_id(&answer.task_id) {
        return Err(ClientError::bad(
            CLIENT,
            "the task's id is not one Tangible uses",
        ));
    }
    Ok(Task {
        phase: TaskPhase::from_word(&answer.status),
        state: answer.status.chars().take(32).collect(),
        stats: answer.meta.and_then(|meta| meta.scan_stats),
        id: answer.task_id,
    })
}

/// Whether `id` looks like a task id RomM makes (a UUID), and so may go in
/// a path.
#[must_use]
pub fn is_task_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// A RomM's API.
pub struct RommClient {
    http: OutboundClient,
    base: Url,
    token: SecretString,
}

impl fmt::Debug for RommClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RommClient")
            .field("base", &crate::outbound::redacted(&self.base))
            .finish_non_exhaustive()
    }
}

/// An answer: its status and its body.
struct Answer {
    status: u16,
    text: String,
}

impl RommClient {
    /// A client for the RomM at `base`, signing in with client token
    /// `token`.
    #[must_use]
    pub fn new(http: OutboundClient, mut base: Url, token: SecretString) -> Self {
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Self { http, base, token }
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url, ClientError> {
        let mut url = self
            .base
            .join(path)
            .map_err(|_| ClientError::bad(CLIENT, "the address cannot be joined"))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        url: &Url,
        signed: bool,
        body: Option<serde_json::Value>,
    ) -> Result<Answer, ClientError> {
        let mut request = self.http.request(method, url).map_err(outbound)?;
        if signed {
            // reqwest marks the header sensitive.
            request = request.bearer_auth(self.token.expose());
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status().as_u16();
        let text = read_text(CLIENT, response).await?;
        if signed && status == 401 {
            return Err(ClientError::Auth {
                client: CLIENT,
                reason: "the client token is wrong, expired or revoked",
            });
        }
        Ok(Answer { status, text })
    }

    /// A signed GET that must answer 200.
    async fn read(
        &self,
        path: &str,
        query: &[(&str, &str)],
        scope: Scope,
    ) -> Result<String, ClientError> {
        let url = self.url(path, query)?;
        let answer = self.send(reqwest::Method::GET, &url, true, None).await?;
        match answer.status {
            200 => Ok(answer.text),
            403 => Err(missing(scope)),
            code => Err(ClientError::bad(CLIENT, format!("{path} answered {code}"))),
        }
    }

    /// RomM's version, such as `5.4.1`. It needs no token.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn version(&self) -> Result<String, ClientError> {
        let url = self.url("api/heartbeat", &[])?;
        let answer = self.send(reqwest::Method::GET, &url, false, None).await?;
        if answer.status != 200 {
            return Err(ClientError::bad(
                CLIENT,
                format!("the heartbeat answered {}; is this RomM?", answer.status),
            ));
        }
        parse_version(&answer.text)
    }

    /// Which of the operations Tangible uses it has. It needs no token.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn capabilities(&self) -> Result<Capabilities, ClientError> {
        let url = self.url("openapi.json", &[])?;
        let answer = self.send(reqwest::Method::GET, &url, false, None).await?;
        if answer.status != 200 {
            return Err(ClientError::bad(
                CLIENT,
                format!("its API document answered {}", answer.status),
            ));
        }
        capabilities(&answer.text)
    }

    /// Whether its configuration leaves the export's marker out of a game's
    /// files. It needs no token.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn ignores_marker(&self) -> Result<bool, ClientError> {
        let url = self.url("api/config", &[])?;
        let answer = self.send(reqwest::Method::GET, &url, false, None).await?;
        if answer.status != 200 {
            return Err(ClientError::bad(
                CLIENT,
                format!("its configuration answered {}", answer.status),
            ));
        }
        parse_ignores_marker(&answer.text)
    }

    /// Whether the token carries `scope`, by a request that needs it and
    /// changes nothing.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; [`ClientError::Auth`] for a token that does not
    /// open at all.
    pub async fn carries(&self, scope: Scope) -> Result<bool, ClientError> {
        let (path, query) = scope.proof();
        let url = self.url(path, query)?;
        let answer = self.send(reqwest::Method::GET, &url, true, None).await?;
        match answer.status {
            200 => Ok(true),
            403 => Ok(false),
            code => Err(ClientError::bad(CLIENT, format!("{path} answered {code}"))),
        }
    }

    /// Its version, what it offers, which scopes the token carries, and
    /// whether it leaves the export's marker out of a game's files.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; a configuration that cannot be read is
    /// [`Probe::ignores_marker`] `None`, not an error.
    pub async fn probe(&self) -> Result<Probe, ClientError> {
        let version = self.version().await?;
        let capabilities = self.capabilities().await?;
        let mut scopes = Vec::with_capacity(Scope::ALL.len());
        for scope in Scope::ALL {
            scopes.push((scope, self.carries(scope).await?));
        }
        let ignores_marker = match self.ignores_marker().await {
            Ok(ignores) => Some(ignores),
            Err(error) => {
                tracing::debug!(error = %error, "RomM's configuration could not be read");
                None
            }
        };
        Ok(Probe {
            version,
            capabilities,
            scopes,
            ignores_marker,
        })
    }

    /// The platforms it has games for.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn platforms(&self) -> Result<Vec<Platform>, ClientError> {
        parse_platforms(
            &self
                .read("api/platforms", &[], Scope::PlatformsRead)
                .await?,
        )
    }

    /// The slugs of every platform it knows.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn known_platforms(&self) -> Result<Vec<String>, ClientError> {
        parse_known_platforms(
            &self
                .read("api/platforms/supported", &[], Scope::PlatformsRead)
                .await?,
        )
    }

    /// A page of a platform's ROMs with their files, oldest first, matching
    /// `search` (fuzzily) when given.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn roms(
        &self,
        platform_id: i64,
        search: Option<&str>,
        offset: u32,
        limit: u32,
    ) -> Result<RomPage, ClientError> {
        let platform = platform_id.to_string();
        let offset = offset.to_string();
        let limit = limit.clamp(1, MAX_PAGE).to_string();
        let mut query = vec![
            ("platform_ids", platform.as_str()),
            ("with_files", "true"),
            ("order_by", "id"),
            ("order_dir", "asc"),
            ("limit", limit.as_str()),
            ("offset", offset.as_str()),
        ];
        if let Some(search) = search {
            query.push(("search_term", search));
        }
        parse_rom_page(&self.read("api/roms", &query, Scope::RomsRead).await?)
    }

    /// ROM `id`, or `None` when RomM has no such ROM.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn rom(&self, id: i64) -> Result<Option<Rom>, ClientError> {
        let url = self.url(&format!("api/roms/{id}"), &[])?;
        let answer = self.send(reqwest::Method::GET, &url, true, None).await?;
        match answer.status {
            200 => parse_rom(&answer.text).map(Some),
            404 => Ok(None),
            403 => Err(missing(Scope::RomsRead)),
            code => Err(ClientError::bad(CLIENT, format!("a ROM answered {code}"))),
        }
    }

    /// Queue a quick scan of the platform folders `fs_slugs`, answering the
    /// task. A quick scan adds what is new and notices what moved or went;
    /// it does not fetch metadata again for what RomM has.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn scan(&self, fs_slugs: &[String]) -> Result<Task, ClientError> {
        let url = self.url("api/tasks/scan", &[])?;
        let body = serde_json::json!({ "type": "quick", "platform_fs_slugs": fs_slugs });
        let answer = self
            .send(reqwest::Method::POST, &url, true, Some(body))
            .await?;
        match answer.status {
            200 | 202 => parse_task(&answer.text),
            403 => Err(missing(Scope::TasksRun)),
            code => Err(ClientError::bad(
                CLIENT,
                format!("asking for a scan answered {code}"),
            )),
        }
    }

    /// Task `id`, or `None` when RomM no longer has it.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn task(&self, id: &str) -> Result<Option<Task>, ClientError> {
        if !is_task_id(id) {
            return Err(ClientError::bad(CLIENT, "that is not a task id"));
        }
        let url = self.url(&format!("api/tasks/{id}"), &[])?;
        let answer = self.send(reqwest::Method::GET, &url, true, None).await?;
        match answer.status {
            200 => parse_task(&answer.text).map(Some),
            404 => Ok(None),
            403 => Err(missing(Scope::TasksRun)),
            code => Err(ClientError::bad(CLIENT, format!("a task answered {code}"))),
        }
    }
}

/// The token lacks `scope`.
const fn missing(scope: Scope) -> ClientError {
    ClientError::Auth {
        client: CLIENT,
        reason: match scope {
            Scope::PlatformsRead => "the client token lacks the platforms.read scope",
            Scope::RomsRead => "the client token lacks the roms.read scope",
            Scope::TasksRun => "the client token lacks the tasks.run scope",
        },
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../../fixtures/tool-output/romm/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn the_version_is_read_from_the_heartbeat() {
        assert_eq!(
            parse_version(&fixture("heartbeat-5.4.1.json")).unwrap(),
            "5.4.1"
        );
        assert!(parse_version("<html>").is_err());
        assert!(parse_version(r#"{"SYSTEM": {}}"#).is_err());
    }

    #[test]
    fn a_5_4_document_offers_everything_tangible_uses() {
        assert_eq!(
            capabilities(&fixture("openapi-5.4.1-used.json")).unwrap(),
            Capabilities {
                client_tokens: true,
                lookup: true,
                known_platforms: true,
                scan: true,
            }
        );
    }

    #[test]
    fn what_a_document_lacks_is_what_cannot_be_used() {
        let full: serde_json::Value =
            serde_json::from_str(&fixture("openapi-5.4.1-used.json")).unwrap();
        // RomM 5.0 to 5.3: no scan a token may ask for.
        let mut before_scans = full.clone();
        before_scans["paths"]
            .as_object_mut()
            .unwrap()
            .remove("/api/tasks/scan");
        let found = capabilities(&before_scans.to_string()).unwrap();
        assert!(found.lookup && found.client_tokens && !found.scan);
        // Before 5.0: no client tokens.
        let mut before_tokens = full.clone();
        before_tokens["paths"]
            .as_object_mut()
            .unwrap()
            .remove("/api/client-tokens");
        assert!(
            !capabilities(&before_tokens.to_string())
                .unwrap()
                .client_tokens
        );
        // A route without a parameter Tangible sends.
        let mut no_search = full;
        no_search["paths"]["/api/roms"]["get"]["parameters"]
            .as_array_mut()
            .unwrap()
            .retain(|parameter| parameter["name"] != "search_term");
        let found = capabilities(&no_search.to_string()).unwrap();
        assert!(!found.lookup && found.scan);
        for bad in [
            "",
            "[]",
            r#"{"swagger": "2.0", "paths": {}}"#,
            r#"{"openapi": "3.1.0"}"#,
        ] {
            assert!(capabilities(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn platforms_and_roms_parse_from_what_romm_answered() {
        let platforms = parse_platforms(&fixture("platforms-5.4.1.json")).unwrap();
        assert_eq!(
            platforms,
            [Platform {
                id: 1,
                slug: "psx".to_owned(),
                fs_slug: "psx".to_owned(),
            }]
        );
        let page = parse_rom_page(&fixture("roms-search-5.4.1.json")).unwrap();
        assert_eq!(page.total, Some(1));
        let rom = &page.items[0];
        assert_eq!(
            (rom.id, rom.platform_id, rom.platform_fs_slug.as_str()),
            (1, 1, "psx")
        );
        assert_eq!(
            (
                rom.fs_path.as_str(),
                rom.fs_name.as_str(),
                rom.missing_from_fs
            ),
            ("roms/psx", "Probe Game (USA)", false)
        );
        let mut files: Vec<(&str, i64)> = rom
            .files
            .iter()
            .map(|file| (file.file_name.as_str(), file.file_size_bytes))
            .collect();
        files.sort_unstable();
        assert_eq!(
            files,
            [
                ("Probe Game (Disc 1).bin", 2_408_448),
                ("Probe Game (Disc 1).cue", 85),
            ]
        );
        assert!(
            rom.files
                .iter()
                .all(|file| file.file_path == "roms/psx/Probe Game (USA)")
        );
        assert!(parse_rom_page(r#"{"items": [{"id": 1}]}"#).is_err());
    }

    #[test]
    fn the_configuration_says_whether_the_marker_is_left_out() {
        let config = fixture("config-5.4.1.json");
        assert!(parse_ignores_marker(&config).unwrap());
        let mut without: serde_json::Value = serde_json::from_str(&config).unwrap();
        without["EXCLUDED_MULTI_PARTS_FILES"]
            .as_array_mut()
            .unwrap()
            .retain(|name| name != crate::romm::MARKER);
        assert!(!parse_ignores_marker(&without.to_string()).unwrap());
        let shouted = r#"{"EXCLUDED_MULTI_PARTS_FILES": [" .TANGIBLE-EXPORT.JSON "]}"#;
        assert!(parse_ignores_marker(shouted).unwrap());
        assert!(parse_ignores_marker("{}").is_err());
    }

    #[test]
    fn a_scan_is_followed_from_queued_to_finished() {
        let queued = parse_task(&fixture("scan-queued-5.4.1.json")).unwrap();
        assert_eq!(queued.id, "32708cfa-50ff-4401-9e6b-adc8b5b301d6");
        assert_eq!(
            (queued.phase, queued.state.as_str(), queued.stats),
            (TaskPhase::Waiting, "queued", None)
        );
        let early = parse_task(&fixture("task-early-5.4.1.json")).unwrap();
        assert_eq!(early.phase, TaskPhase::Running);
        let finished = parse_task(&fixture("task-finished-5.4.1.json")).unwrap();
        assert_eq!(finished.phase, TaskPhase::Finished);
        assert!(finished.phase.ended());
        assert_eq!(
            finished.stats,
            Some(ScanStats {
                total_roms: 1,
                scanned_roms: 1,
                new_roms: 0,
                updated_roms: 0,
            })
        );
        for (word, phase) in [
            ("failed", TaskPhase::Failed),
            ("canceled", TaskPhase::Canceled),
            ("stopped", TaskPhase::Canceled),
            ("scheduled", TaskPhase::Waiting),
            ("something new", TaskPhase::Waiting),
        ] {
            assert_eq!(TaskPhase::from_word(word), phase, "{word}");
        }
        let odd_id = r#"{"task_id": "../../api/users", "status": "queued"}"#;
        assert!(parse_task(odd_id).is_err());
    }

    #[test]
    fn a_probe_says_what_can_be_done() {
        let all = Capabilities {
            client_tokens: true,
            lookup: true,
            known_platforms: true,
            scan: true,
        };
        let probe = |capabilities, granted: &[Scope]| Probe {
            version: "5.4.1".to_owned(),
            capabilities,
            scopes: Scope::ALL
                .iter()
                .map(|scope| (*scope, granted.contains(scope)))
                .collect(),
            ignores_marker: Some(true),
        };
        let everything = probe(all, &Scope::ALL);
        assert!(everything.can_link() && everything.can_scan());
        let no_tasks = probe(all, &[Scope::PlatformsRead, Scope::RomsRead]);
        assert!(no_tasks.can_link() && !no_tasks.can_scan());
        let no_roms = probe(all, &[Scope::PlatformsRead, Scope::TasksRun]);
        assert!(!no_roms.can_link() && !no_roms.can_scan());
        let old = probe(Capabilities { scan: false, ..all }, &Scope::ALL);
        assert!(old.can_link() && !old.can_scan());
        let ancient = probe(Capabilities::default(), &Scope::ALL);
        assert!(!ancient.can_link());
    }

    #[test]
    fn the_token_is_never_in_debug_output() {
        let http = OutboundClient::new(
            crate::outbound::OutboundPolicy::for_integrations(Vec::new(), None),
            "tangible-test",
        )
        .unwrap();
        let client = RommClient::new(
            http,
            Url::parse("http://romm:8080").unwrap(),
            SecretString::from("rmm_secret-token-value".to_owned()),
        );
        let shown = format!("{client:?}");
        assert!(!shown.contains("rmm_secret"), "{shown}");
        assert!(shown.contains("http://romm:8080/"), "{shown}");
    }
}
