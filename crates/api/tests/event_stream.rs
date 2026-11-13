// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The event stream against a real PostgreSQL over a real socket
//! (API.md 12, docs/plans/event-stream.md).
//!
//! Changes are made the way any code path makes them, by writing rows; what
//! is asserted is that the stream says so, only for what committed, and that
//! a client that reconnects is sent what it missed.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use sqlx::{Executor as _, PgPool};
use tangible_api::{ApiState, serve};
use tangible_db::{Database, DbConfig};
use tangible_domain::{Role, WorkerId};

mod support;

struct Server {
    base: String,
    pool: PgPool,
    session: support::Session,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

async fn server() -> Server {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    let pool = database.pool().clone();
    let session = support::sign_in(&pool, Role::Viewer).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(serve(listener, ApiState::new(database), async {
        let _ = stopped.await;
    }));
    Server {
        base,
        pool,
        session,
        stop: Some(stop),
        task: Some(task),
    }
}

impl Server {
    async fn open(&self, query: &str, last_event_id: Option<&str>) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .get(format!("{}/api/v1/events{query}", self.base))
            .header("cookie", self.session.cookie());
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        request.send().await.expect("request")
    }

    async fn stream(&self, query: &str, last_event_id: Option<&str>) -> Stream {
        let response = self.open(query, last_event_id).await;
        assert_eq!(response.status(), 200);
        assert!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream")),
            "{:?}",
            response.headers()
        );
        Stream {
            response,
            buffer: String::new(),
        }
    }

    /// A worker row, which notifies `worker.changed`.
    async fn add_worker(&self) -> WorkerId {
        let worker = WorkerId::generate();
        self.pool
            .execute(
                sqlx::AssertSqlSafe(format!(
                    "INSERT INTO workers (id, name, software_version, protocol_version, credential_hash) \
                     VALUES ('{worker}', 'stream-{worker}', '0.1.0', '1alpha1', 'stream-{worker}')"
                )
                .as_str()),
            )
            .await
            .expect("insert");
        worker
    }

    async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            // Shutdown ends open streams, so this does not wait for them.
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("the server stops promptly")
                .expect("the server task")
                .expect("served");
        }
    }
}

#[derive(Debug, Default)]
struct SseEvent {
    id: Option<String>,
    event: Option<String>,
    data: serde_json::Value,
}

struct Stream {
    response: reqwest::Response,
    buffer: String,
}

impl Stream {
    /// The next event within `within`, keep-alive comments skipped.
    async fn next(&mut self, within: Duration) -> Option<SseEvent> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                let mut event = SseEvent::default();
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(value) = line.strip_prefix("id:") {
                        event.id = Some(value.trim().to_owned());
                    } else if let Some(value) = line.strip_prefix("event:") {
                        event.event = Some(value.trim().to_owned());
                    } else if let Some(value) = line.strip_prefix("data:") {
                        data.push_str(value.trim());
                    }
                }
                if event.event.is_none() && data.is_empty() {
                    continue;
                }
                event.data = serde_json::from_str(&data).expect("JSON data");
                return Some(event);
            }
            let chunk = tokio::time::timeout_at(deadline, self.response.chunk())
                .await
                .ok()?
                .expect("read")?;
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// The next `worker.changed` event for `worker`, skipping others.
    async fn worker(&mut self, worker: WorkerId, within: Duration) -> Option<SseEvent> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = self.next(left).await?;
            if event.event.as_deref() == Some("worker.changed")
                && event.data["id"] == worker.to_string()
            {
                return Some(event);
            }
        }
    }
}

/// Add workers until one is heard of: the server begins listening a moment
/// after it starts, and a change made before that is not news to it.
async fn first_heard(server: &Server, stream: &mut Stream) -> (WorkerId, SseEvent) {
    for _ in 0..50 {
        let worker = server.add_worker().await;
        if let Some(event) = stream.worker(worker, Duration::from_millis(400)).await {
            return (worker, event);
        }
    }
    panic!("the stream never said a worker was added");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_committed_change_is_sent_with_its_topic_and_identifiers() {
    let mut server = server().await;
    let mut stream = server.stream("?topics=workers", None).await;
    let (worker, event) = first_heard(&server, &mut stream).await;
    assert!(event.id.is_some(), "events carry ids");
    assert_eq!(event.data["topic"], "workers");
    assert_eq!(event.data["id"], worker.to_string());
    assert_eq!(event.data["status"], "pending");
    // Nothing secret travels: only the columns the trigger names.
    assert!(
        event.data.get("credential_hash").is_none(),
        "{:?}",
        event.data
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_change_rolled_back_is_never_sent() {
    let mut server = server().await;
    let mut stream = server.stream("?topics=workers", None).await;
    first_heard(&server, &mut stream).await;

    let rolled_back = WorkerId::generate();
    let mut tx = server.pool.begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO workers (id, name, software_version, protocol_version, credential_hash) \
         VALUES ($1, $2, '0.1.0', '1alpha1', $2)",
    )
    .bind(rolled_back.as_uuid())
    .bind(format!("rolled-back-{rolled_back}"))
    .execute(&mut *tx)
    .await
    .expect("insert");
    tx.rollback().await.expect("rollback");
    let committed = server.add_worker().await;

    // The next worker event is the committed one; the other never comes.
    let mut seen = Vec::new();
    while let Some(event) = stream.next(Duration::from_secs(2)).await {
        if event.event.as_deref() == Some("worker.changed") {
            seen.push(event.data["id"].as_str().unwrap_or_default().to_owned());
            if seen.contains(&committed.to_string()) {
                break;
            }
        }
    }
    assert!(seen.contains(&committed.to_string()), "{seen:?}");
    assert!(!seen.contains(&rolled_back.to_string()), "{seen:?}");
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_reconnecting_client_is_sent_what_it_missed_or_told_to_reconcile() {
    let mut server = server().await;
    let mut stream = server.stream("?topics=workers", None).await;
    let (_, first) = first_heard(&server, &mut stream).await;
    let first_id = first.id.expect("an id");
    drop(stream);

    // Missed while away, and replayed on reconnecting.
    let missed = server.add_worker().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut again = server.stream("?topics=workers", Some(&first_id)).await;
    let replayed = again
        .worker(missed, Duration::from_secs(5))
        .await
        .expect("the missed change is replayed");
    assert!(replayed.id.is_some());
    drop(again);

    // The same, named by `after`.
    let mut after = server
        .stream(&format!("?topics=workers&after={first_id}"), None)
        .await;
    after
        .worker(missed, Duration::from_secs(5))
        .await
        .expect("replayed for `after` too");
    drop(after);

    // An id the server does not hold: reconcile.
    let mut unknown = server
        .stream(
            "?topics=workers",
            Some("0198aaaa-0000-7000-8000-000000000000"),
        )
        .await;
    let reset = unknown
        .next(Duration::from_secs(5))
        .await
        .expect("an event");
    assert_eq!(reset.event.as_deref(), Some("stream.reset"));
    assert!(
        reset.id.is_none(),
        "about this stream, not something that happened"
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_stream_needs_a_session_and_known_topics() {
    let mut server = server().await;
    let unauthenticated = reqwest::Client::new()
        .get(format!("{}/api/v1/events", server.base))
        .send()
        .await
        .expect("request");
    assert_eq!(unauthenticated.status(), 401);
    let unknown = server.open("?topics=burns,secrets", None).await;
    assert_eq!(unknown.status(), 400);
    let body: serde_json::Value = unknown.json().await.expect("problem");
    assert_eq!(body["code"], "INVALID_PARAMETER");
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn shutting_down_ends_open_streams() {
    let mut server = server().await;
    let mut stream = server.stream("", None).await;
    let began = std::time::Instant::now();
    // stop() waits at most ten seconds for the server, which would otherwise
    // wait out the stream's ten minutes.
    server.stop().await;
    assert!(stream.next(Duration::from_secs(5)).await.is_none());
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "the stream ended at shutdown rather than timing out"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn one_account_cannot_take_every_place() {
    let mut server = server().await;
    let mut held = Vec::new();
    for _ in 0..16 {
        let response = server.open("", None).await;
        assert_eq!(response.status(), 200);
        held.push(response);
    }
    let refused = server.open("", None).await;
    assert_eq!(refused.status(), 429);
    let body: serde_json::Value = refused.json().await.expect("problem");
    assert_eq!(body["code"], "RATE_LIMITED");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("this account"),
        "{body}"
    );

    // Somebody else still gets one, and a place closed is given back.
    let other = support::sign_in(&server.pool, Role::Viewer).await;
    let theirs = reqwest::Client::new()
        .get(format!("{}/api/v1/events", server.base))
        .header("cookie", other.cookie())
        .send()
        .await
        .expect("request");
    assert_eq!(theirs.status(), 200);
    drop(held.pop());
    let mut given_back = false;
    for _ in 0..50 {
        let response = server.open("", None).await;
        if response.status() == 200 {
            given_back = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(given_back, "the closed stream's place came back");
    drop(theirs);
    drop(held);
    server.stop().await;
}
