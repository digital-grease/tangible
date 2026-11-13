// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Requests for items (ADR 0021, docs/plans/requests.md).
//!
//! Somebody signed in asks for an item; an operator or administrator approves
//! or declines it; nothing is searched for before approval. Every change is
//! made under the request's row lock, follows `RequestState::can_become`, and
//! writes its audit row in the same transaction. The attestation a request is
//! made with is not stored (the owner's choice).

use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::str::FromStr;
use tangible_domain::{RequestId, RequestKind, RequestState};
use time::OffsetDateTime;

use crate::DbError;
use crate::accounts::audit;

/// A request, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    /// The request.
    pub id: RequestId,
    /// The account that asked.
    pub requester_id: uuid::Uuid,
    /// Its username.
    pub requested_by: String,
    /// What kind of item.
    pub kind: RequestKind,
    /// The item, as a person sees it.
    pub title: String,
    /// Platform, year, region, edition and the like.
    pub details: Value,
    /// Where the item was found: `[{"provider", "external_id"}]`.
    pub refs: Value,
    /// What the requester added.
    pub note: Option<String>,
    /// Where it stands.
    pub state: RequestState,
    /// Who approved or declined it.
    pub decided_by: Option<String>,
    /// When.
    pub decided_at: Option<OffsetDateTime>,
    /// Why it was declined.
    pub decline_reason: Option<String>,
    /// The download client a release went to.
    pub integration_id: Option<uuid::Uuid>,
    /// The import following it.
    pub import_id: Option<uuid::Uuid>,
    /// What fulfilled it.
    pub artifact_id: Option<uuid::Uuid>,
    /// Why the last download failed, as a stable code.
    pub error_code: Option<String>,
    /// And in words.
    pub error_detail: Option<String>,
    /// When it was asked for.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

/// A request to record.
#[derive(Debug, Clone)]
pub struct NewRequest<'a> {
    /// The account asking.
    pub requester_id: uuid::Uuid,
    /// Its username.
    pub requested_by: &'a str,
    /// Whether that account may decide requests: its own are approved as
    /// they are made (docs/plans/requests.md, decision 1).
    pub requester_decides: bool,
    /// What kind of item.
    pub kind: RequestKind,
    /// The item.
    pub title: &'a str,
    /// Platform, year, region, edition.
    pub details: &'a Value,
    /// Where it was found.
    pub refs: &'a Value,
    /// A note.
    pub note: Option<&'a str>,
}

/// Which requests to list.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestFilter {
    /// One account's only; `None` for everybody's.
    pub requester_id: Option<uuid::Uuid>,
    /// One state only.
    pub state: Option<RequestState>,
}

/// Why a request could not be changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRefusal {
    /// No such request, or not one the caller may see.
    NotFound,
    /// It is in a state that does not allow the change.
    Conflict(RequestState),
}

/// A decision on a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision<'a> {
    /// Approve it: it may be searched for.
    Approve,
    /// Decline it, saying why.
    Decline(&'a str),
}

/// The columns every read returns.
const COLUMNS: &str = "id, requester_id, requested_by, kind, title, details, refs, note, state, \
                       decided_by, decided_at, decline_reason, integration_id, import_id, \
                       artifact_id, error_code, error_detail, created_at, updated_at";

fn parse<T: FromStr>(column: &'static str, value: &str) -> Result<T, DbError> {
    T::from_str(value).map_err(|_| DbError::Enum {
        column,
        value: value.to_owned(),
    })
}

fn request_from_row(row: &sqlx::postgres::PgRow) -> Result<RequestRecord, DbError> {
    let get = |e| DbError::Query(e);
    let kind: String = row.try_get("kind").map_err(get)?;
    let state: String = row.try_get("state").map_err(get)?;
    Ok(RequestRecord {
        id: RequestId::from_uuid(row.try_get("id").map_err(get)?),
        requester_id: row.try_get("requester_id").map_err(get)?,
        requested_by: row.try_get("requested_by").map_err(get)?,
        kind: parse("requests.kind", &kind)?,
        title: row.try_get("title").map_err(get)?,
        details: row.try_get("details").map_err(get)?,
        refs: row.try_get("refs").map_err(get)?,
        note: row.try_get("note").map_err(get)?,
        state: parse("requests.state", &state)?,
        decided_by: row.try_get("decided_by").map_err(get)?,
        decided_at: row.try_get("decided_at").map_err(get)?,
        decline_reason: row.try_get("decline_reason").map_err(get)?,
        integration_id: row.try_get("integration_id").map_err(get)?,
        import_id: row.try_get("import_id").map_err(get)?,
        artifact_id: row.try_get("artifact_id").map_err(get)?,
        error_code: row.try_get("error_code").map_err(get)?,
        error_detail: row.try_get("error_detail").map_err(get)?,
        created_at: row.try_get("created_at").map_err(get)?,
        updated_at: row.try_get("updated_at").map_err(get)?,
    })
}

async fn request_in(
    tx: &mut Transaction<'_, Postgres>,
    id: RequestId,
) -> Result<RequestRecord, DbError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM requests WHERE id = $1"
    )))
    .bind(id.as_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    request_from_row(&row)
}

/// Record a request, and that it was made; one made by somebody who may
/// decide requests is approved as it is made, and says so.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, including a field the schema
/// refuses.
pub async fn create_request(pool: &PgPool, new: &NewRequest<'_>) -> Result<RequestRecord, DbError> {
    let id = RequestId::generate();
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let (state, decided_by) = if new.requester_decides {
        (RequestState::Approved, Some(new.requested_by))
    } else {
        (RequestState::PendingApproval, None)
    };
    sqlx::query(
        "INSERT INTO requests
             (id, requester_id, requested_by, kind, title, details, refs, note, state,
              decided_by, decided_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                 CASE WHEN $10::text IS NULL THEN NULL ELSE now() END)",
    )
    .bind(id.as_uuid())
    .bind(new.requester_id)
    .bind(new.requested_by)
    .bind(new.kind.as_str())
    .bind(new.title)
    .bind(new.details)
    .bind(new.refs)
    .bind(new.note)
    .bind(state.as_str())
    .bind(decided_by)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    audit(
        &mut tx,
        new.requested_by,
        "request.created",
        &id.to_string(),
        serde_json::json!({ "kind": new.kind.as_str(), "title": new.title }),
    )
    .await?;
    if new.requester_decides {
        audit(
            &mut tx,
            new.requested_by,
            "request.approved",
            &id.to_string(),
            serde_json::json!({ "own_request": true }),
        )
        .await?;
    }
    let record = request_in(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(record)
}

/// One request.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_request(pool: &PgPool, id: RequestId) -> Result<Option<RequestRecord>, DbError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM requests WHERE id = $1"
    )))
    .bind(id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(request_from_row).transpose()
}

/// Requests, newest first, after the cursor `before` (a request's id).
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_requests(
    pool: &PgPool,
    filter: RequestFilter,
    before: Option<uuid::Uuid>,
    limit: i64,
) -> Result<Vec<RequestRecord>, DbError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM requests
         WHERE ($1::uuid IS NULL OR requester_id = $1)
           AND ($2::text IS NULL OR state = $2)
           AND ($3::uuid IS NULL OR id < $3)
         ORDER BY id DESC
         LIMIT $4"
    )))
    .bind(filter.requester_id)
    .bind(filter.state.map(|state| state.as_str()))
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(request_from_row).collect()
}

/// Lock a request and read its state; `None` when it does not exist, or
/// belongs to another account when `requester_id` is given.
async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    id: RequestId,
    requester_id: Option<uuid::Uuid>,
) -> Result<Option<RequestState>, DbError> {
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM requests
         WHERE id = $1 AND ($2::uuid IS NULL OR requester_id = $2)
         FOR UPDATE",
    )
    .bind(id.as_uuid())
    .bind(requester_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    state
        .map(|state| parse("requests.state", &state))
        .transpose()
}

/// Approve or decline a request.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn decide_request(
    pool: &PgPool,
    id: RequestId,
    actor: &str,
    decision: Decision<'_>,
) -> Result<Result<RequestRecord, RequestRefusal>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some(state) = lock(&mut tx, id, None).await? else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RequestRefusal::NotFound));
    };
    let (next, reason, action) = match decision {
        Decision::Approve => (RequestState::Approved, None, "request.approved"),
        Decision::Decline(reason) => (RequestState::Declined, Some(reason), "request.declined"),
    };
    if !state.can_become(next) {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RequestRefusal::Conflict(state)));
    }
    sqlx::query(
        "UPDATE requests
         SET state = $2, decided_by = $3, decided_at = now(), decline_reason = $4
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(next.as_str())
    .bind(actor)
    .bind(reason)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let metadata = match reason {
        Some(reason) => serde_json::json!({ "reason": reason }),
        None => serde_json::json!({}),
    };
    audit(&mut tx, actor, action, &id.to_string(), metadata).await?;
    let record = request_in(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

/// Withdraw a request: the requester's own (`requester_id` given), or any,
/// for somebody who may decide requests.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn cancel_request(
    pool: &PgPool,
    id: RequestId,
    actor: &str,
    requester_id: Option<uuid::Uuid>,
) -> Result<Result<RequestRecord, RequestRefusal>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some(state) = lock(&mut tx, id, requester_id).await? else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RequestRefusal::NotFound));
    };
    if !state.can_become(RequestState::Canceled) {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RequestRefusal::Conflict(state)));
    }
    sqlx::query("UPDATE requests SET state = 'canceled' WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    audit(
        &mut tx,
        actor,
        "request.canceled",
        &id.to_string(),
        serde_json::json!({}),
    )
    .await?;
    let record = request_in(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}
