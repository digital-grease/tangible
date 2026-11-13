// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The metrics listener over a real socket, read from a real PostgreSQL
//! (docs/plans/metrics.md).
//!
//! The database is shared with every other test, so these look only for
//! what they wrote: a worker with a name nothing else uses, removed again.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use sqlx::Executor as _;
use tangible_api::metrics::{Metrics, REUSE_FOR, serve};

mod support;

struct Listening {
    base: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn listen(token: Option<&str>) -> (Listening, tangible_db::Database) {
    let database = support::database().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let metrics = Metrics::new(database.clone(), token.map(ToOwned::to_owned));
    let task = tokio::spawn(serve(listener, metrics, async {
        let _ = stopped.await;
    }));
    (
        Listening {
            base,
            stop: Some(stop),
            task,
        },
        database,
    )
}

impl Listening {
    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap().unwrap();
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_scrape_reads_the_catalogue_in_the_text_format() {
    let (listening, database) = listen(None).await;
    let id = uuid::Uuid::now_v7();
    let name = format!("metrics-{}", id.simple());
    database
        .pool()
        .execute(sqlx::AssertSqlSafe(
            format!(
                "INSERT INTO workers (id, name, status, software_version, protocol_version,
                                      credential_hash, capabilities_json, last_seen_at)
                 VALUES ('{id}', '{name}', 'online', '0.1.0', '1alpha1', 'hash-{id}', '{{}}',
                         now() - interval '30 seconds')"
            )
            .as_str(),
        ))
        .await
        .expect("a worker");

    let response = reqwest::get(format!("{}/metrics", listening.base))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let text = response.text().await.unwrap();
    for family in [
        "tangible_build_info",
        "tangible_artifacts",
        "tangible_stored_bytes",
        "tangible_jobs",
        "tangible_burn_attempt_seconds",
        "tangible_workers",
        "tangible_artifact_integrity",
        "tangible_preservation_hashes",
        "tangible_dats",
        "tangible_metrics_scrape_seconds",
    ] {
        assert!(
            text.contains(&format!("# TYPE {family} ")),
            "{family} in\n{text}"
        );
    }
    let seen = text
        .lines()
        .find(|line| {
            line.starts_with(&format!(
                "tangible_worker_last_seen_seconds{{worker=\"{name}\"}}"
            ))
        })
        .unwrap_or_else(|| panic!("the worker in\n{text}"));
    let seconds: f64 = seen.rsplit_once(' ').unwrap().1.parse().unwrap();
    assert!((29.0..120.0).contains(&seconds), "{seen}");

    // A second scrape at once is the first one again: one set of queries.
    let again = reqwest::get(format!("{}/metrics", listening.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(again, text);
    assert!(REUSE_FOR < Duration::from_secs(60));

    // Anything but /metrics is not there.
    let other = reqwest::get(format!("{}/api/v1/artifacts", listening.base))
        .await
        .unwrap();
    assert_eq!(other.status(), 404);

    database
        .pool()
        .execute(sqlx::AssertSqlSafe(
            format!("DELETE FROM workers WHERE id = '{id}'").as_str(),
        ))
        .await
        .expect("remove the worker");
    listening.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn with_a_token_a_scrape_must_carry_it() {
    let (listening, _) = listen(Some("s3cret-metrics-token")).await;
    let client = reqwest::Client::new();
    let url = format!("{}/metrics", listening.base);

    let refused = client.get(&url).send().await.unwrap();
    assert_eq!(refused.status(), 401);
    assert!(refused.headers().contains_key("www-authenticate"));
    let wrong = client
        .get(&url)
        .bearer_auth("not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let body = wrong.text().await.unwrap();
    assert!(!body.contains("tangible_"), "nothing leaks: {body}");

    let allowed = client
        .get(&url)
        .bearer_auth("s3cret-metrics-token")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    assert!(
        allowed
            .text()
            .await
            .unwrap()
            .contains("# TYPE tangible_jobs gauge")
    );
    listening.stop().await;
}
