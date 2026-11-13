// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! The event stream (API.md 12, docs/plans/event-stream.md).
//!
//! `GET /api/v1/events` is a server-sent event stream of what changes:
//! imports, burns and their progress, rips, erasures, derivations, workers
//! and artifacts. The changes come from the database itself (migration 0031:
//! triggers notify a channel when a change commits), so every change is
//! heard whichever code path made it, and none that was rolled back.
//!
//! An event says what changed and its identifiers and state, never enough
//! to replace reading the resource: a client reconciles through the REST
//! routes. Events carry ids, and a client that reconnects with
//! `Last-Event-ID` is sent what it missed while the server still holds it
//! (the last 1024 events, ten minutes at most); otherwise, or when it falls
//! behind, it is sent `stream.reset` and reconciles. A stream ends after ten
//! minutes, and the browser reconnects, so a session that has ended stops
//! receiving within that.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch};
use utoipa::IntoParams;

use crate::auth::SignedIn;
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// Every topic, in the order a client would name them.
pub const TOPICS: [&str; 7] = [
    "imports",
    "burns",
    "rips",
    "erasures",
    "workers",
    "derivations",
    "library",
];

/// The topic of the events the stream itself sends, which every client gets.
const STREAM_TOPIC: &str = "stream";

/// The event that tells a client it may have missed something.
pub const RESET: &str = "stream.reset";

/// How many events are kept for clients that reconnect.
const RECENT_LIMIT: usize = 1024;

/// How long an event is kept for them.
const RECENT_AGE: Duration = Duration::from_mins(10);

/// How long one stream stays open before it ends and the client reconnects.
const STREAM_LIFETIME: Duration = Duration::from_mins(10);

/// How many streams may be open at once.
const MAX_STREAMS: usize = 256;

/// How many one account may have open: a tab per live page, and some, so
/// one account cannot take every place.
const MAX_STREAMS_PER_ACCOUNT: usize = 16;

/// How many events a slow client may fall behind before it is told to
/// reconcile.
const CHANNEL_CAPACITY: usize = 1024;

/// One event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamEvent {
    /// Opaque, increasing identifier: what `Last-Event-ID` names.
    pub id: String,
    /// The topic: one of [`TOPICS`], or `stream` for the stream's own.
    pub topic: String,
    /// What happened, such as `burn.progress`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Identifiers and state.
    pub data: serde_json::Value,
    #[serde(skip)]
    at: Option<Instant>,
}

/// What a new subscriber is sent before live events.
#[derive(Debug, PartialEq)]
pub(crate) enum Replay {
    /// These, which it missed (none for a new client).
    Events(Vec<Arc<StreamEvent>>),
    /// What it last saw is no longer held: it may have missed something.
    Gap,
}

/// Holds recent events and hands new ones to every open stream.
#[derive(Clone)]
pub struct EventHub {
    inner: Arc<HubInner>,
}

struct HubInner {
    sender: broadcast::Sender<Arc<StreamEvent>>,
    /// Recent events, oldest first. Publishing and subscribing both hold
    /// this, so a subscriber neither misses an event nor gets one twice.
    recent: Mutex<VecDeque<Arc<StreamEvent>>>,
    streams: Arc<Semaphore>,
    /// How many streams each account has open.
    per_account: Mutex<HashMap<uuid::Uuid, usize>>,
    /// True once the server is shutting down: every stream ends, so the
    /// graceful shutdown does not wait out their ten minutes.
    closing: watch::Sender<bool>,
}

impl std::fmt::Debug for EventHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHub").finish_non_exhaustive()
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    /// An empty hub.
    #[must_use]
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(HubInner {
                sender,
                recent: Mutex::new(VecDeque::new()),
                streams: Arc::new(Semaphore::new(MAX_STREAMS)),
                per_account: Mutex::new(HashMap::new()),
                closing: watch::channel(false).0,
            }),
        }
    }

    /// Send an event to every open stream, and keep it for reconnecting
    /// clients.
    pub fn publish(&self, topic: &str, kind: &str, data: serde_json::Value) -> Arc<StreamEvent> {
        let event = Arc::new(StreamEvent {
            id: uuid::Uuid::now_v7().to_string(),
            topic: topic.to_owned(),
            kind: kind.to_owned(),
            data,
            at: Some(Instant::now()),
        });
        let mut recent = self
            .inner
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        recent.push_back(Arc::clone(&event));
        while recent.len() > RECENT_LIMIT
            || recent
                .front()
                .and_then(|oldest| oldest.at)
                .is_some_and(|at| at.elapsed() > RECENT_AGE)
        {
            recent.pop_front();
        }
        // No receivers is not an error: nobody is watching.
        let _ = self.inner.sender.send(Arc::clone(&event));
        event
    }

    /// Tell every client it may have missed something, as after the
    /// database connection was lost.
    pub fn reset(&self, reason: &str) {
        self.publish(STREAM_TOPIC, RESET, serde_json::json!({ "reason": reason }));
    }

    /// Subscribe, with what was missed since `after`.
    pub(crate) fn subscribe(
        &self,
        after: Option<&str>,
    ) -> (Replay, broadcast::Receiver<Arc<StreamEvent>>) {
        let recent = self
            .inner
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let receiver = self.inner.sender.subscribe();
        let replay = match after {
            None => Replay::Events(Vec::new()),
            Some(after) => match recent.iter().position(|event| event.id == after) {
                Some(index) => Replay::Events(recent.iter().skip(index + 1).cloned().collect()),
                None => Replay::Gap,
            },
        };
        (replay, receiver)
    }

    /// End every open stream, and any opened after: the server is shutting
    /// down.
    pub fn close(&self) {
        self.inner.closing.send_replace(true);
    }

    /// A place among the open streams for `account`, if there is one: why
    /// not, if not.
    fn open_stream(&self, account: uuid::Uuid) -> Result<StreamPlace, &'static str> {
        let mut per_account = self
            .inner
            .per_account
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if per_account.get(&account).copied().unwrap_or(0) >= MAX_STREAMS_PER_ACCOUNT {
            return Err("this account has too many event streams open; close a tab or two");
        }
        let permit = Arc::clone(&self.inner.streams)
            .try_acquire_owned()
            .map_err(|_| "too many event streams are open; reload the page later")?;
        *per_account.entry(account).or_insert(0) += 1;
        Ok(StreamPlace {
            _permit: permit,
            account,
            hub: Arc::clone(&self.inner),
        })
    }
}

/// A stream's place, given back when the stream ends.
struct StreamPlace {
    _permit: OwnedSemaphorePermit,
    account: uuid::Uuid,
    hub: Arc<HubInner>,
}

impl Drop for StreamPlace {
    fn drop(&mut self) {
        let mut per_account = self
            .hub
            .per_account
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(open) = per_account.get_mut(&self.account) {
            *open = open.saturating_sub(1);
            if *open == 0 {
                per_account.remove(&self.account);
            }
        }
    }
}

/// Forward the database's changes to `hub` until the process ends.
///
/// A lost connection, or one that cannot be made, is said to every client
/// as `stream.reset`, since changes may have been missed, and retried.
pub async fn forward_changes(database: tangible_db::Database, hub: EventHub) {
    let mut delay = Duration::from_secs(1);
    loop {
        match tangible_db::changes::ChangeListener::connect(database.pool()).await {
            Ok(mut listener) => {
                tracing::info!("listening for changes for the event stream");
                delay = Duration::from_secs(1);
                loop {
                    match listener.next().await {
                        Ok(Some(change)) => {
                            if TOPICS.contains(&change.topic.as_str()) {
                                hub.publish(&change.topic, &change.kind, change.data);
                            }
                        }
                        Ok(None) => {
                            tracing::warn!(
                                "lost the database connection the event stream listens on"
                            );
                            hub.reset("the server lost its database connection for a moment");
                        }
                        Err(error) => {
                            tracing::warn!(error = ?error, "the event stream stopped listening");
                            hub.reset("the server stopped hearing of changes for a moment");
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = ?error, "could not listen for changes for the event stream");
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// Which topics a stream is for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Topics(BTreeSet<&'static str>);

impl Topics {
    /// Parse `imports,burns`; none, or empty, is every topic.
    fn parse(raw: Option<&str>) -> Result<Self, Problem> {
        let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
            return Ok(Self(TOPICS.into_iter().collect()));
        };
        let mut topics = BTreeSet::new();
        for name in raw
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let Some(known) = TOPICS.iter().find(|topic| **topic == name) else {
                return Err(Problem::invalid_parameter(
                    "topics",
                    &format!("one or more of {}", TOPICS.join(", ")),
                ));
            };
            topics.insert(*known);
        }
        Ok(Self(topics))
    }

    fn wants(&self, event: &StreamEvent) -> bool {
        event.topic == STREAM_TOPIC || self.0.contains(event.topic.as_str())
    }
}

/// An event as the stream sends it: its id, its type as the event name, and
/// its topic and data as the data.
fn sse_event(event: &StreamEvent) -> Event {
    let mut data = event.data.clone();
    if let Some(object) = data.as_object_mut() {
        object.insert("topic".to_owned(), serde_json::json!(event.topic));
    }
    Event::default()
        .id(event.id.clone())
        .event(event.kind.clone())
        .data(data.to_string())
}

/// `stream.reset` for one client, which has no id: it is about this
/// stream, not something that happened.
fn reset_event(reason: &str) -> Event {
    Event::default()
        .event(RESET)
        .data(serde_json::json!({ "topic": STREAM_TOPIC, "reason": reason }).to_string())
}

/// The query of `GET /events`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct EventQuery {
    /// Comma-separated topics: imports, burns, rips, erasures, workers,
    /// derivations, library. Every topic when absent.
    pub topics: Option<String>,
    /// The id of the last event seen, as `Last-Event-ID` would say; the
    /// header wins when both are given.
    pub after: Option<String>,
}

struct StreamState {
    pending: VecDeque<Event>,
    receiver: broadcast::Receiver<Arc<StreamEvent>>,
    topics: Topics,
    ends_at: tokio::time::Instant,
    closing: watch::Receiver<bool>,
    _place: StreamPlace,
}

/// What changes, as it changes.
///
/// # Errors
///
/// `INVALID_PARAMETER` for an unknown topic, `RATE_LIMITED` when too many
/// streams are open, or too many of the caller's.
#[utoipa::path(
    get,
    path = "/api/v1/events",
    tag = "events",
    description = "A server-sent event stream (`text/event-stream`) of what changes: \
                   `import.changed`, `burn_job.changed`, `burn_attempt.changed`, \
                   `burn.progress`, `rip.changed`, `rip.progress`, `erasure.changed`, \
                   `derivation.changed`, `worker.changed` and `artifact.changed`, each with \
                   an id and data naming its topic, identifiers and state. Read the \
                   resource itself for the rest. Reconnecting with `Last-Event-ID` (or \
                   `after`) replays what was missed while the server holds it; otherwise, \
                   or when the client falls behind, `stream.reset` is sent and the client \
                   should reconcile through the REST routes. A stream ends after ten \
                   minutes and is reconnected. At most 16 are open for one account, and \
                   256 in all. Needs `read`.",
    params(
        EventQuery,
        ("Last-Event-ID" = Option<String>, Header, description = "The id of the last event seen"),
    ),
    responses(
        (status = 200, description = "The stream", content_type = "text/event-stream", body = String),
        (status = 400, description = "Unknown topic", body = Problem),
        (status = 429, description = "Too many streams are open, or too many of this account's", body = Problem),
    ),
)]
pub async fn event_stream(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let topics = Topics::parse(query.topics.as_deref())?;
    let after = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or(query.after)
        .filter(|after| !after.is_empty() && after.len() <= 64);
    let hub = state.events();
    let place = hub
        .open_stream(user.user_id)
        .map_err(|why| Problem::new(ErrorCode::RateLimited, why))?;
    let (replay, receiver) = hub.subscribe(after.as_deref());
    let pending: VecDeque<Event> = match replay {
        Replay::Events(events) => events
            .iter()
            .filter(|event| topics.wants(event))
            .map(|event| sse_event(event))
            .collect(),
        Replay::Gap => VecDeque::from([reset_event(
            "the events since the one named are no longer held",
        )]),
    };
    let stream_state = StreamState {
        pending,
        receiver,
        topics,
        ends_at: tokio::time::Instant::now() + STREAM_LIFETIME,
        closing: hub.inner.closing.subscribe(),
        _place: place,
    };
    let stream = futures_util::stream::unfold(stream_state, |mut stream_state| async move {
        if let Some(event) = stream_state.pending.pop_front() {
            return Some((Ok::<_, Infallible>(event), stream_state));
        }
        loop {
            if *stream_state.closing.borrow() {
                return None;
            }
            let next = tokio::select! {
                next = tokio::time::timeout_at(
                    stream_state.ends_at,
                    stream_state.receiver.recv(),
                ) => next,
                // Shutting down, or the hub is gone: end now.
                _ = stream_state.closing.changed() => return None,
            };
            match next {
                // Its time is up: the browser reconnects, signed in or not.
                Err(_) | Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                Ok(Ok(event)) => {
                    if stream_state.topics.wants(&event) {
                        return Some((Ok(sse_event(&event)), stream_state));
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    return Some((Ok(reset_event("this stream fell behind")), stream_state));
                }
            }
        }
    });

    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    // A reverse proxy that buffers would hold events back until it had a
    // bufferful.
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

/// The event stream's routes.
pub fn router() -> Router<ApiState> {
    Router::new().route("/events", get(event_stream))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ids(replay: &Replay) -> Vec<String> {
        match replay {
            Replay::Events(events) => events.iter().map(|event| event.id.clone()).collect(),
            Replay::Gap => panic!("a gap"),
        }
    }

    #[test]
    fn a_new_subscriber_is_sent_nothing_old() {
        let hub = EventHub::new();
        hub.publish(
            "burns",
            "burn_job.changed",
            serde_json::json!({ "id": "j" }),
        );
        let (replay, _receiver) = hub.subscribe(None);
        assert_eq!(replay, Replay::Events(Vec::new()));
    }

    #[test]
    fn a_reconnecting_subscriber_is_sent_what_it_missed() {
        let hub = EventHub::new();
        let first = hub.publish("burns", "burn_job.changed", serde_json::json!({}));
        let second = hub.publish("imports", "import.changed", serde_json::json!({}));
        let third = hub.publish("burns", "burn.progress", serde_json::json!({}));
        let (replay, _receiver) = hub.subscribe(Some(&first.id));
        assert_eq!(ids(&replay), [second.id.clone(), third.id.clone()]);
        let (replay, _receiver) = hub.subscribe(Some(&third.id));
        assert_eq!(ids(&replay), Vec::<String>::new());
    }

    #[test]
    fn an_id_no_longer_held_is_a_gap() {
        let hub = EventHub::new();
        let first = hub.publish("burns", "burn_job.changed", serde_json::json!({}));
        for _ in 0..RECENT_LIMIT {
            hub.publish("burns", "burn.progress", serde_json::json!({}));
        }
        let (replay, _receiver) = hub.subscribe(Some(&first.id));
        assert_eq!(replay, Replay::Gap, "the oldest is dropped past the limit");
        let (replay, _receiver) = hub.subscribe(Some("not-an-id"));
        assert_eq!(replay, Replay::Gap);
    }

    #[tokio::test]
    async fn a_subscriber_receives_what_is_published_after_it() {
        let hub = EventHub::new();
        let (_, mut receiver) = hub.subscribe(None);
        let event = hub.publish(
            "rips",
            "rip.progress",
            serde_json::json!({ "progress": 0.5 }),
        );
        assert_eq!(receiver.recv().await.unwrap().id, event.id);
    }

    #[test]
    fn ids_increase() {
        let hub = EventHub::new();
        let a = hub.publish("burns", "x", serde_json::json!({}));
        let b = hub.publish("burns", "x", serde_json::json!({}));
        assert!(b.id > a.id);
    }

    #[test]
    fn topics_are_parsed_and_unknown_ones_refused() {
        assert_eq!(Topics::parse(None).unwrap().0.len(), TOPICS.len());
        assert_eq!(Topics::parse(Some("")).unwrap().0.len(), TOPICS.len());
        let topics = Topics::parse(Some("burns, imports")).unwrap();
        assert_eq!(topics.0, BTreeSet::from(["burns", "imports"]));
        assert!(Topics::parse(Some("burns,secrets")).is_err());
    }

    #[test]
    fn the_streams_own_events_reach_every_stream() {
        let topics = Topics::parse(Some("burns")).unwrap();
        let hub = EventHub::new();
        let burn = hub.publish("burns", "burn.progress", serde_json::json!({}));
        let import = hub.publish("imports", "import.changed", serde_json::json!({}));
        hub.reset("testing");
        let (_, _receiver) = hub.subscribe(None);
        assert!(topics.wants(&burn));
        assert!(!topics.wants(&import));
        let recent = hub.inner.recent.lock().unwrap();
        let reset = recent.back().unwrap();
        assert_eq!(reset.kind, RESET);
        assert!(topics.wants(reset));
    }

    #[tokio::test]
    async fn closing_ends_the_streams() {
        let hub = EventHub::new();
        let mut closing = hub.inner.closing.subscribe();
        assert!(!*closing.borrow());
        hub.close();
        closing.changed().await.unwrap();
        assert!(*closing.borrow());
    }

    #[test]
    fn places_are_limited_in_all() {
        let hub = EventHub::new();
        let held: Vec<_> = (0..MAX_STREAMS)
            .map(|_| hub.open_stream(uuid::Uuid::now_v7()).expect("a place"))
            .collect();
        assert!(hub.open_stream(uuid::Uuid::now_v7()).is_err());
        drop(held);
        assert!(hub.open_stream(uuid::Uuid::now_v7()).is_ok());
        assert!(
            hub.inner.per_account.lock().unwrap().is_empty(),
            "none left counted"
        );
    }

    #[test]
    fn places_are_limited_for_one_account() {
        let hub = EventHub::new();
        let account = uuid::Uuid::now_v7();
        let mut held: Vec<_> = (0..MAX_STREAMS_PER_ACCOUNT)
            .map(|_| hub.open_stream(account).expect("a place"))
            .collect();
        let refused = hub.open_stream(account).err().expect("refused");
        assert!(refused.contains("this account"), "{refused}");
        // Another account is not held back by it.
        assert!(hub.open_stream(uuid::Uuid::now_v7()).is_ok());
        held.pop();
        assert!(hub.open_stream(account).is_ok(), "a place given back");
    }
}
