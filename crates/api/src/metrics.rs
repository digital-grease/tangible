// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prometheus metrics (docs/plans/metrics.md): `GET /metrics` on a listener
//! of its own, off unless `TANGIBLE_METRICS_BIND` names one.
//!
//! Everything is read from the catalogue at scrape time (see
//! `tangible_db::metrics`) and written in the Prometheus text format, 0.0.4,
//! by hand: a few lines per family need no client library. A scrape is
//! reused for a few seconds, so several scrapers cost one set of queries.
//! With a token configured, a scrape must carry it as a bearer token.

use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use subtle::ConstantTimeEq as _;
use tangible_db::Database;
use tangible_db::metrics::{MetricsSnapshot, snapshot};
use tokio::sync::Mutex;

/// How long one scrape's answer is reused.
pub const REUSE_FOR: Duration = Duration::from_secs(5);

/// The content type of the text format.
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The metrics listener's state.
#[derive(Clone)]
pub struct Metrics {
    database: Database,
    token: Option<Arc<str>>,
    last: Arc<Mutex<Option<(Instant, String)>>>,
}

impl Metrics {
    /// Metrics read from `database`, and the token a scrape must carry, if
    /// any.
    #[must_use]
    pub fn new(database: Database, token: Option<String>) -> Self {
        Self {
            database,
            token: token.map(Arc::from),
            last: Arc::new(Mutex::new(None)),
        }
    }

    /// The text of a scrape: the last one if it is recent, otherwise read
    /// now. One at a time, so scrapes arriving together share one reading.
    async fn text(&self) -> Result<String, tangible_db::DbError> {
        let mut last = self.last.lock().await;
        if let Some((at, text)) = last.as_ref()
            && at.elapsed() < REUSE_FOR
        {
            return Ok(text.clone());
        }
        let started = Instant::now();
        let read = snapshot(self.database.pool()).await?;
        let text = render(&read, started.elapsed().as_secs_f64());
        *last = Some((Instant::now(), text.clone()));
        Ok(text)
    }

    /// Whether the request carries the token, or none is needed.
    fn allowed(&self, headers: &HeaderMap) -> bool {
        let Some(token) = self.token.as_deref() else {
            return true;
        };
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|supplied| bool::from(supplied.as_bytes().ct_eq(token.as_bytes())))
    }
}

async fn scrape(State(metrics): State<Metrics>, headers: HeaderMap) -> Response {
    if !metrics.allowed(&headers) {
        let mut response =
            (StatusCode::UNAUTHORIZED, "a bearer token is required\n").into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"tangible-metrics\""),
        );
        return response;
    }
    match metrics.text().await {
        Ok(text) => ([(header::CONTENT_TYPE, CONTENT_TYPE)], text).into_response(),
        Err(error) => {
            tracing::warn!(error = ?error, "metrics could not be read");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the database could not be read; see server logs\n",
            )
                .into_response()
        }
    }
}

/// The metrics router: `GET /metrics`, and nothing else.
pub fn router(metrics: Metrics) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(metrics)
}

/// Serve metrics on `listener` until `shutdown` completes.
///
/// # Errors
///
/// An I/O error from the listener.
pub async fn serve<S>(
    listener: tokio::net::TcpListener,
    metrics: Metrics,
    shutdown: S,
) -> std::io::Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(metrics))
        .with_graceful_shutdown(shutdown)
        .await
}

/// A label value as the text format writes one: backslash, double quote and
/// line feed escaped.
fn escape_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// The text format, one family at a time.
struct Exposition(String);

impl Exposition {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}");
        let _ = writeln!(self.0, "# TYPE {name} {kind}");
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.0.push_str(name);
        if !labels.is_empty() {
            self.0.push('{');
            for (index, (label, value)) in labels.iter().enumerate() {
                if index > 0 {
                    self.0.push(',');
                }
                let _ = write!(self.0, "{label}=\"{}\"", escape_label(value));
            }
            self.0.push('}');
        }
        let _ = writeln!(self.0, " {value}");
    }

    /// A family of one label, a sample for each value.
    fn by(&mut self, name: &str, kind: &str, help: &str, label: &str, rows: &[(String, i64)]) {
        self.family(name, kind, help);
        for (value, count) in rows {
            #[allow(clippy::cast_precision_loss)]
            self.sample(name, &[(label, value)], *count as f64);
        }
    }

    /// A family of one sample with no labels.
    fn one(&mut self, name: &str, kind: &str, help: &str, value: f64) {
        self.family(name, kind, help);
        self.sample(name, &[], value);
    }
}

/// Write a snapshot in the text format; `scrape_seconds` is how long reading
/// it took.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn render(read: &MetricsSnapshot, scrape_seconds: f64) -> String {
    let mut out = Exposition(String::new());
    out.family(
        "tangible_build_info",
        "gauge",
        "The running version, as a label.",
    );
    out.sample(
        "tangible_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );
    out.by(
        "tangible_artifacts",
        "gauge",
        "Artifacts in the catalogue, by format.",
        "format",
        &read.artifacts_by_format,
    );
    out.one(
        "tangible_stored_objects",
        "gauge",
        "Objects in the content-addressed store.",
        read.stored_objects as f64,
    );
    out.one(
        "tangible_stored_bytes",
        "gauge",
        "Bytes in the content-addressed store, each object once.",
        read.stored_bytes as f64,
    );

    library(&mut out, read);
    hardware(&mut out, read);
    checks(&mut out, read);
    out.one(
        "tangible_metrics_scrape_seconds",
        "gauge",
        "How long reading these metrics from the database took.",
        scrape_seconds,
    );
    out.0
}

/// Jobs and how long they wait.
#[allow(clippy::cast_precision_loss)]
fn library(out: &mut Exposition, read: &MetricsSnapshot) {
    out.family("tangible_jobs", "gauge", "Jobs of each kind, by state.");
    for (kind, state, count) in &read.jobs {
        out.sample(
            "tangible_jobs",
            &[("kind", kind), ("state", state)],
            *count as f64,
        );
    }
    out.family(
        "tangible_oldest_waiting_seconds",
        "gauge",
        "Age of the oldest job of each kind not yet started, for kinds with one waiting.",
    );
    for (kind, seconds) in &read.oldest_waiting {
        out.sample(
            "tangible_oldest_waiting_seconds",
            &[("kind", kind)],
            *seconds,
        );
    }
}

/// Burns, workers and drives.
#[allow(clippy::cast_precision_loss)]
fn hardware(out: &mut Exposition, read: &MetricsSnapshot) {
    out.by(
        "tangible_burn_attempts",
        "gauge",
        "Burn attempts, by state.",
        "state",
        &read.burn_attempts,
    );
    out.family(
        "tangible_burn_attempt_seconds",
        "summary",
        "How long finished burn attempts took, from claim to end.",
    );
    out.sample(
        "tangible_burn_attempt_seconds_sum",
        &[],
        read.burn_attempt_seconds.0,
    );
    out.sample(
        "tangible_burn_attempt_seconds_count",
        &[],
        read.burn_attempt_seconds.1 as f64,
    );

    out.by(
        "tangible_workers",
        "gauge",
        "Workers, by status.",
        "status",
        &read.workers,
    );
    out.family(
        "tangible_worker_last_seen_seconds",
        "gauge",
        "Seconds since each worker was last heard from.",
    );
    for (worker, seconds) in &read.worker_last_seen {
        out.sample(
            "tangible_worker_last_seen_seconds",
            &[("worker", worker)],
            *seconds,
        );
    }
    out.by(
        "tangible_drives",
        "gauge",
        "Drives, by status.",
        "status",
        &read.drives,
    );
}

/// Integrity, identification and integrations.
#[allow(clippy::cast_precision_loss)]
fn checks(out: &mut Exposition, read: &MetricsSnapshot) {
    out.by(
        "tangible_artifact_integrity",
        "gauge",
        "Artifacts, by what the last integrity check covering them found.",
        "state",
        &read.integrity,
    );
    if let Some(at) = read.last_full_check {
        out.one(
            "tangible_last_full_check_timestamp_seconds",
            "gauge",
            "When the last check of the whole library completed, in Unix time.",
            at,
        );
    }

    out.by(
        "tangible_preservation_hashes",
        "gauge",
        "Artifacts, by the state of their preservation hashes.",
        "state",
        &read.preservation_hashes,
    );
    out.one(
        "tangible_dats",
        "gauge",
        "Preservation DATs loaded.",
        read.dats as f64,
    );
    out.by(
        "tangible_dat_matches",
        "gauge",
        "Matches of artifacts to games of loaded DATs, by kind.",
        "kind",
        &read.dat_matches,
    );

    out.family(
        "tangible_integrations",
        "gauge",
        "Integrations, by kind and status.",
    );
    for (kind, status, count) in &read.integrations {
        out.sample(
            "tangible_integrations",
            &[("kind", kind), ("status", status)],
            *count as f64,
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn read() -> MetricsSnapshot {
        MetricsSnapshot {
            artifacts_by_format: vec![("cue_bin".into(), 3), ("iso".into(), 2)],
            stored_objects: 9,
            stored_bytes: 1_000_000,
            jobs: vec![
                ("import", "complete".into(), 5),
                ("burn", "queued".into(), 1),
            ],
            oldest_waiting: vec![("burn", 42.5)],
            burn_attempts: vec![("verified".into(), 4)],
            burn_attempt_seconds: (600.0, 4),
            workers: vec![("online".into(), 1)],
            worker_last_seen: vec![("drive \"one\"\\\n".into(), 3.0)],
            drives: vec![("ready_empty".into(), 1)],
            integrity: vec![("intact".into(), 5)],
            last_full_check: Some(1_793_000_000.0),
            preservation_hashes: vec![("done".into(), 5)],
            dats: 2,
            dat_matches: vec![("exact".into(), 3)],
            integrations: vec![("qbittorrent".into(), "ok".into(), 1)],
        }
    }

    #[test]
    fn writes_each_family_with_its_help_and_type() {
        let text = render(&read(), 0.012);
        assert!(text.contains(
            "# HELP tangible_artifacts Artifacts in the catalogue, by format.\n\
             # TYPE tangible_artifacts gauge\n\
             tangible_artifacts{format=\"cue_bin\"} 3\n\
             tangible_artifacts{format=\"iso\"} 2\n"
        ));
        assert!(text.contains("tangible_stored_bytes 1000000\n"));
        assert!(text.contains("tangible_jobs{kind=\"burn\",state=\"queued\"} 1\n"));
        assert!(text.contains("tangible_oldest_waiting_seconds{kind=\"burn\"} 42.5\n"));
        assert!(text.contains("# TYPE tangible_burn_attempt_seconds summary\n"));
        assert!(text.contains("tangible_burn_attempt_seconds_sum 600\n"));
        assert!(text.contains("tangible_burn_attempt_seconds_count 4\n"));
        assert!(text.contains("tangible_last_full_check_timestamp_seconds 1793000000\n"));
        assert!(text.contains("tangible_integrations{kind=\"qbittorrent\",status=\"ok\"} 1\n"));
        assert!(text.contains(&format!(
            "tangible_build_info{{version=\"{}\"}} 1\n",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(text.ends_with("tangible_metrics_scrape_seconds 0.012\n"));
        // Every sample line is a name, optional labels, and a number.
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (_, value) = line.rsplit_once(' ').expect("a value");
            assert!(value.parse::<f64>().is_ok(), "{line}");
        }
    }

    #[test]
    fn escapes_label_values_a_worker_named_oddly_cannot_break_out_of() {
        let text = render(&read(), 0.0);
        assert!(
            text.contains(
                "tangible_worker_last_seen_seconds{worker=\"drive \\\"one\\\"\\\\\\n\"} 3\n"
            ),
            "{text}"
        );
        assert_eq!(text.lines().filter(|l| l.contains("worker=")).count(), 1);
    }

    #[test]
    fn leaves_out_a_full_check_that_never_happened() {
        let text = render(
            &MetricsSnapshot {
                last_full_check: None,
                ..read()
            },
            0.0,
        );
        assert!(!text.contains("tangible_last_full_check_timestamp_seconds"));
    }
}
