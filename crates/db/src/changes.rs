// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Changes the database announces (migration 0031, docs/plans/event-stream.md).
//!
//! Triggers on the job, event, worker and artifact tables notify one channel
//! with a topic, an event type and the row's identifiers and state, delivered
//! when the change commits. This listens to that channel on a connection of
//! its own and hands each change on, parsed.

use sqlx::PgPool;
use sqlx::postgres::PgListener;

use crate::DbError;

/// The channel the triggers notify.
pub const CHANNEL: &str = "tangible_events";

/// The longest notification read; PostgreSQL's own limit is 8000 bytes.
const PAYLOAD_LIMIT: usize = 8000;

/// One change, as a trigger announced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// What it is about: `imports`, `burns`, `rips`, `erasures`, `workers`,
    /// `derivations` or `library`.
    pub topic: String,
    /// What happened, such as `burn.progress` or `import.changed`.
    pub kind: String,
    /// Identifiers and state, as the trigger chose them.
    pub data: serde_json::Value,
}

/// Listens for changes on a connection of its own.
pub struct ChangeListener {
    listener: PgListener,
}

impl std::fmt::Debug for ChangeListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeListener").finish_non_exhaustive()
    }
}

impl ChangeListener {
    /// Start listening.
    ///
    /// # Errors
    ///
    /// [`DbError::Query`] if the connection or the `LISTEN` fails.
    pub async fn connect(pool: &PgPool) -> Result<Self, DbError> {
        let mut listener = PgListener::connect_with(pool)
            .await
            .map_err(DbError::Query)?;
        listener.listen(CHANNEL).await.map_err(DbError::Query)?;
        Ok(Self { listener })
    }

    /// The next change.
    ///
    /// `Ok(None)` when the connection was lost: changes made meanwhile were
    /// not heard, and the next call connects again and listens.
    ///
    /// # Errors
    ///
    /// [`DbError::Query`] if listening again fails.
    pub async fn next(&mut self) -> Result<Option<Change>, DbError> {
        loop {
            let Some(notification) = self.listener.try_recv().await.map_err(DbError::Query)? else {
                return Ok(None);
            };
            if let Some(change) = parse(notification.payload()) {
                return Ok(Some(change));
            }
            // Only the triggers notify this channel; anything else is
            // somebody's mistake, and skipped.
            tracing::warn!("a change notification could not be read; skipped");
        }
    }
}

/// A notification's payload, if it is one the triggers write.
#[must_use]
pub fn parse(payload: &str) -> Option<Change> {
    if payload.len() > PAYLOAD_LIMIT {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let topic = value.get("topic")?.as_str()?.to_owned();
    let kind = value.get("type")?.as_str()?.to_owned();
    let data = value.get("data")?.clone();
    if !data.is_object() || topic.is_empty() || kind.is_empty() {
        return None;
    }
    Some(Change { topic, kind, data })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_trigger_payload_is_read() {
        let change = parse(
            r#"{"topic":"burns","type":"burn_job.changed","data":{"id":"j","state":"queued"}}"#,
        )
        .expect("a change");
        assert_eq!(change.topic, "burns");
        assert_eq!(change.kind, "burn_job.changed");
        assert_eq!(change.data["state"], "queued");
    }

    #[test]
    fn anything_else_is_not() {
        for payload in [
            "",
            "not json",
            r#"{"topic":"burns"}"#,
            r#"{"topic":"burns","type":"x","data":[1]}"#,
            r#"{"topic":"","type":"x","data":{}}"#,
            r#"{"topic":1,"type":"x","data":{}}"#,
        ] {
            assert_eq!(parse(payload), None, "{payload}");
        }
        let long = format!(
            r#"{{"topic":"burns","type":"x","data":{{"pad":"{}"}}}}"#,
            "a".repeat(9000)
        );
        assert_eq!(parse(&long), None);
    }
}
