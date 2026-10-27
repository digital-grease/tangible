// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A fake RomM, answering as RomM 5.4.1 did (fixtures/tool-output/romm):
//! its version, document and configuration without a token, and with one
//! its platforms, its ROMs (searched, paged, or by id), scans asked for and
//! the tasks that follow them. Tests change what it holds and answers
//! through [`Shared`].

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};

/// The token the fake opens to.
pub const TOKEN: &str = "rmm_fake-token-for-the-fake-romm";

/// The id of every scan task it queues.
pub const TASK: &str = "32708cfa-50ff-4401-9e6b-adc8b5b301d6";

/// A recorded RomM 5.4.1 answer.
pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../fixtures/tool-output/romm/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|error| panic!("fixture {name}: {error}"))
}

/// How the fake answers a request for a scan.
#[derive(Debug, Clone, Copy)]
pub enum ScanReply {
    /// 202, queued.
    Queued,
    /// 409, a scan already running.
    Busy,
    /// 503, no scan worker.
    NoWorker,
}

/// How the fake answers after a task.
#[derive(Debug, Clone, Copy)]
pub enum TaskReply {
    /// 200 with this status.
    Says(&'static str),
    /// 404.
    Gone,
}

/// A ROM the fake holds: one game folder.
#[derive(Debug, Clone)]
pub struct FakeRom {
    /// RomM's id.
    pub id: i64,
    /// The platform's id.
    pub platform_id: i64,
    /// The platform's folder.
    pub platform: String,
    /// The game folder's name.
    pub fs_name: String,
    /// Whether a scan found it gone.
    pub missing: bool,
    /// Its files, names and sizes.
    pub files: Vec<(String, i64)>,
}

impl FakeRom {
    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "platform_id": self.platform_id,
            "platform_fs_slug": self.platform,
            "fs_name": self.fs_name,
            "fs_path": format!("roms/{}", self.platform),
            "missing_from_fs": self.missing,
            "files": self.files.iter().map(|(name, size)| json!({
                "file_name": name,
                "file_path": format!("roms/{}/{}", self.platform, self.fs_name),
                "file_size_bytes": size,
            })).collect::<Vec<_>>(),
        })
    }
}

/// What the fake answers, and what it was asked.
pub struct Fake {
    /// Its OpenAPI document.
    pub document: Value,
    /// Answers to requests for scans, in turn; queued once they run out.
    pub scan_replies: VecDeque<ScanReply>,
    /// Answers about tasks, in turn; finished once they run out.
    pub task_replies: VecDeque<TaskReply>,
    /// While set, a task is `started` whatever else is queued.
    pub hold: bool,
    /// The bodies of the scans asked for.
    pub scans: Vec<Value>,
    /// How many times a task was asked after.
    pub task_asks: usize,
    /// Its platforms: id and folder.
    pub platforms: Vec<(i64, String)>,
    /// The platform slugs it knows.
    pub known: Vec<String>,
    /// Its ROMs.
    pub roms: Vec<FakeRom>,
    /// Every signed request, as `METHOD path?query`.
    pub asked: Vec<String>,
    /// When set, a search finds nothing, as RomM's fuzzy search may not.
    pub blind_search: bool,
}

/// The fake, shared with the test.
pub type Shared = Arc<Mutex<Fake>>;

fn task(status: &str) -> String {
    if status == "finished" {
        return fixture("task-finished-5.4.1.json");
    }
    json!({ "task_id": TASK, "status": status, "meta": { "scan_stats": null } }).to_string()
}

fn query(uri: &axum::http::Uri) -> Vec<(String, String)> {
    tangible_integrations::outbound::Url::parse(&format!("http://fake{uri}"))
        .map(|url| url.query_pairs().into_owned().collect())
        .unwrap_or_default()
}

fn rom_page(fake: &Fake, uri: &axum::http::Uri) -> Value {
    let query = query(uri);
    let get = |key: &str| {
        query
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    let platform: Option<i64> = get("platform_ids").and_then(|id| id.parse().ok());
    let search = get("search_term").map(|term| term.to_lowercase());
    if search.is_some() && fake.blind_search {
        return json!({ "items": [], "total": 0, "limit": 50, "offset": 0 });
    }
    let offset: usize = get("offset").and_then(|n| n.parse().ok()).unwrap_or(0);
    let limit: usize = get("limit").and_then(|n| n.parse().ok()).unwrap_or(50);
    let mut matching: Vec<&FakeRom> = fake
        .roms
        .iter()
        .filter(|rom| platform.is_none_or(|id| rom.platform_id == id))
        // RomM's search is fuzzy; a substring stands in for it.
        .filter(|rom| {
            search
                .as_ref()
                .is_none_or(|term| rom.fs_name.to_lowercase().contains(term.as_str()))
        })
        .collect();
    matching.sort_by_key(|rom| rom.id);
    let total = matching.len();
    json!({
        "items": matching.iter().skip(offset).take(limit).map(|rom| rom.json()).collect::<Vec<_>>(),
        "total": total,
        "limit": limit,
        "offset": offset,
    })
}

async fn answer(
    State(fake): State<Shared>,
    method: Method,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: String,
) -> axum::response::Response {
    let path = uri.path().to_owned();
    let mut fake = fake
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match path.as_str() {
        "/api/heartbeat" => {
            return (StatusCode::OK, fixture("heartbeat-5.4.1.json")).into_response();
        }
        "/openapi.json" => return (StatusCode::OK, fake.document.to_string()).into_response(),
        "/api/config" => return (StatusCode::OK, fixture("config-5.4.1.json")).into_response(),
        _ => {}
    }
    let signed = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(format!("Bearer {TOKEN}").as_str());
    if !signed {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "detail": "Not authenticated" })),
        )
            .into_response();
    }
    fake.asked.push(format!(
        "{method} {path}{}",
        uri.query().map(|q| format!("?{q}")).unwrap_or_default()
    ));
    let task_path = format!("/api/tasks/{TASK}");
    match (method.as_str(), path.as_str()) {
        ("GET", "/api/platforms") => {
            let platforms: Vec<Value> = fake
                .platforms
                .iter()
                .map(|(id, slug)| json!({ "id": id, "slug": slug, "fs_slug": slug }))
                .collect();
            (StatusCode::OK, axum::Json(platforms)).into_response()
        }
        ("GET", "/api/platforms/supported") => {
            let known: Vec<Value> = fake
                .known
                .iter()
                .map(|slug| json!({ "id": -1, "slug": slug, "fs_slug": slug }))
                .collect();
            (StatusCode::OK, axum::Json(known)).into_response()
        }
        ("GET", "/api/roms") => (StatusCode::OK, axum::Json(rom_page(&fake, &uri))).into_response(),
        ("GET", rom) if rom.starts_with("/api/roms/") => {
            let id: Option<i64> = rom.trim_start_matches("/api/roms/").parse().ok();
            match fake.roms.iter().find(|rom| Some(rom.id) == id) {
                Some(rom) => (StatusCode::OK, axum::Json(rom.json())).into_response(),
                None => (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({ "detail": "Rom not found" })),
                )
                    .into_response(),
            }
        }
        ("POST", "/api/tasks/scan") => {
            fake.scans
                .push(serde_json::from_str(&body).unwrap_or(Value::Null));
            match fake.scan_replies.pop_front().unwrap_or(ScanReply::Queued) {
                ScanReply::Queued => {
                    (StatusCode::ACCEPTED, fixture("scan-queued-5.4.1.json")).into_response()
                }
                ScanReply::Busy => {
                    (StatusCode::CONFLICT, fixture("scan-in-flight-5.4.1.json")).into_response()
                }
                ScanReply::NoWorker => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    fixture("scan-no-worker-5.4.1.json"),
                )
                    .into_response(),
            }
        }
        ("GET", asked) if asked == task_path => {
            fake.task_asks += 1;
            if fake.hold {
                return (StatusCode::OK, task("started")).into_response();
            }
            match fake
                .task_replies
                .pop_front()
                .unwrap_or(TaskReply::Says("finished"))
            {
                TaskReply::Says(status) => (StatusCode::OK, task(status)).into_response(),
                TaskReply::Gone => (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({ "detail": "Task not found" })),
                )
                    .into_response(),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Start a fake RomM on a loopback port, holding nothing.
pub async fn start() -> (SocketAddr, Shared) {
    let fake: Shared = Arc::new(Mutex::new(Fake {
        document: serde_json::from_str(&fixture("openapi-5.4.1-used.json")).unwrap_or(Value::Null),
        scan_replies: VecDeque::new(),
        task_replies: VecDeque::new(),
        hold: false,
        scans: Vec::new(),
        task_asks: 0,
        platforms: Vec::new(),
        known: Vec::new(),
        roms: Vec::new(),
        asked: Vec::new(),
        blind_search: false,
    }));
    let app = axum::Router::new()
        .fallback(answer)
        .with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("binding the fake RomM: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("the fake RomM's address: {error}"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, fake)
}
