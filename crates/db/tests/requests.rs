// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Requests against a real PostgreSQL (ADR 0021, docs/plans/requests.md).

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::PgPool;
use tangible_db::accounts::create_user;
use tangible_db::requests::{
    Decision, NewRequest, RequestFilter, RequestRefusal, cancel_request, create_request,
    decide_request, get_request, list_requests,
};
use tangible_db::{Database, DbConfig};
use tangible_domain::{RequestKind, RequestState, Role};

async fn pool() -> PgPool {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    database.pool().clone()
}

/// An account of its own for one test: (id, username).
async fn account(pool: &PgPool, role: Role) -> (uuid::Uuid, String) {
    let name = format!("req-{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
    let user = create_user(pool, &name, "$argon2id$stand-in", role, "test")
        .await
        .unwrap()
        .unwrap();
    (user.id, name)
}

async fn ask(pool: &PgPool, who: &(uuid::Uuid, String), decides: bool) -> uuid::Uuid {
    let details = serde_json::json!({ "platform": "Sony PlayStation", "region": "USA" });
    let refs = serde_json::json!([{ "provider": "dat", "external_id": "x" }]);
    let record = create_request(
        pool,
        &NewRequest {
            requester_id: who.0,
            requested_by: &who.1,
            requester_decides: decides,
            kind: RequestKind::Game,
            title: "Some Game",
            details: &details,
            refs: &refs,
            note: Some("the disc is scratched"),
        },
    )
    .await
    .unwrap();
    *record.id.as_uuid()
}

async fn audit_actions(pool: &PgPool, id: uuid::Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM audit_events WHERE target_id = $1 ORDER BY id")
        .bind(id.to_string())
        .fetch_all(pool)
        .await
        .unwrap()
}

fn rid(id: uuid::Uuid) -> tangible_domain::RequestId {
    tangible_domain::RequestId::from_uuid(id)
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_viewers_request_waits_and_a_deciders_is_approved_as_made() {
    let pool = pool().await;
    let viewer = account(&pool, Role::Viewer).await;
    let operator = account(&pool, Role::Operator).await;

    let waiting = ask(&pool, &viewer, false).await;
    let record = get_request(&pool, rid(waiting)).await.unwrap().unwrap();
    assert_eq!(record.state, RequestState::PendingApproval);
    assert_eq!(record.decided_by, None);
    assert_eq!(record.requested_by, viewer.1);
    assert_eq!(record.details["platform"], "Sony PlayStation");
    assert_eq!(audit_actions(&pool, waiting).await, ["request.created"]);

    let own = ask(&pool, &operator, true).await;
    let record = get_request(&pool, rid(own)).await.unwrap().unwrap();
    assert_eq!(record.state, RequestState::Approved);
    assert_eq!(record.decided_by.as_deref(), Some(operator.1.as_str()));
    assert!(record.decided_at.is_some());
    assert_eq!(
        audit_actions(&pool, own).await,
        ["request.created", "request.approved"]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_requester_sees_their_own_and_a_decider_sees_all() {
    let pool = pool().await;
    let one = account(&pool, Role::Viewer).await;
    let other = account(&pool, Role::Viewer).await;
    let mine = ask(&pool, &one, false).await;
    let theirs = ask(&pool, &other, false).await;

    let own = list_requests(
        &pool,
        RequestFilter {
            requester_id: Some(one.0),
            state: None,
        },
        None,
        100,
    )
    .await
    .unwrap();
    assert_eq!(own.len(), 1);
    assert_eq!(*own[0].id.as_uuid(), mine);

    let all = list_requests(&pool, RequestFilter::default(), None, 1000)
        .await
        .unwrap();
    let ids: Vec<_> = all.iter().map(|r| *r.id.as_uuid()).collect();
    assert!(ids.contains(&mine) && ids.contains(&theirs));
    // Newest first, and a cursor continues after it.
    assert!(ids.iter().position(|id| *id == theirs) < ids.iter().position(|id| *id == mine));
    let after = list_requests(&pool, RequestFilter::default(), Some(theirs), 1000)
        .await
        .unwrap();
    assert!(after.iter().all(|r| *r.id.as_uuid() < theirs));

    let pending = list_requests(
        &pool,
        RequestFilter {
            requester_id: Some(one.0),
            state: Some(RequestState::Approved),
        },
        None,
        100,
    )
    .await
    .unwrap();
    assert!(pending.is_empty(), "filtered by state");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_decision_follows_the_states_and_is_audited() {
    let pool = pool().await;
    let viewer = account(&pool, Role::Viewer).await;
    let operator = account(&pool, Role::Operator).await;

    let approved = ask(&pool, &viewer, false).await;
    let record = decide_request(&pool, rid(approved), &operator.1, Decision::Approve)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.state, RequestState::Approved);
    assert_eq!(record.decided_by.as_deref(), Some(operator.1.as_str()));
    // Approving twice is refused, saying where it is.
    assert_eq!(
        decide_request(&pool, rid(approved), &operator.1, Decision::Approve)
            .await
            .unwrap(),
        Err(RequestRefusal::Conflict(RequestState::Approved))
    );

    let declined = ask(&pool, &viewer, false).await;
    let record = decide_request(
        &pool,
        rid(declined),
        &operator.1,
        Decision::Decline("we have it already"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(record.state, RequestState::Declined);
    assert_eq!(record.decline_reason.as_deref(), Some("we have it already"));
    assert_eq!(
        audit_actions(&pool, declined).await,
        ["request.created", "request.declined"]
    );
    // Nothing leaves an end.
    assert_eq!(
        decide_request(&pool, rid(declined), &operator.1, Decision::Approve)
            .await
            .unwrap(),
        Err(RequestRefusal::Conflict(RequestState::Declined))
    );
    assert_eq!(
        decide_request(
            &pool,
            rid(uuid::Uuid::now_v7()),
            &operator.1,
            Decision::Approve
        )
        .await
        .unwrap(),
        Err(RequestRefusal::NotFound)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_requester_withdraws_only_their_own() {
    let pool = pool().await;
    let one = account(&pool, Role::Viewer).await;
    let other = account(&pool, Role::Viewer).await;
    let operator = account(&pool, Role::Operator).await;
    let mine = ask(&pool, &one, false).await;

    // Another viewer is told there is no such request.
    assert_eq!(
        cancel_request(&pool, rid(mine), &other.1, Some(other.0))
            .await
            .unwrap(),
        Err(RequestRefusal::NotFound)
    );
    let record = cancel_request(&pool, rid(mine), &one.1, Some(one.0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.state, RequestState::Canceled);
    assert_eq!(
        cancel_request(&pool, rid(mine), &one.1, Some(one.0))
            .await
            .unwrap(),
        Err(RequestRefusal::Conflict(RequestState::Canceled))
    );

    // A decider may withdraw anybody's.
    let theirs = ask(&pool, &other, false).await;
    assert!(
        cancel_request(&pool, rid(theirs), &operator.1, None)
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_schema_refuses_what_the_states_forbid() {
    let pool = pool().await;
    let viewer = account(&pool, Role::Viewer).await;
    let id = ask(&pool, &viewer, false).await;
    for statement in [
        // Declined says why, and who.
        "UPDATE requests SET state = 'declined', decided_by = 'x', decided_at = now() WHERE id = $1",
        // Approved names who decided.
        "UPDATE requests SET state = 'approved' WHERE id = $1",
        // A download follows an import.
        "UPDATE requests SET state = 'downloading', decided_by = 'x', decided_at = now() WHERE id = $1",
        // Fulfilled names an artifact.
        "UPDATE requests SET state = 'fulfilled', decided_by = 'x', decided_at = now() WHERE id = $1",
        // A decision is whole.
        "UPDATE requests SET decided_by = 'x' WHERE id = $1",
        // An unknown kind or state.
        "UPDATE requests SET state = 'searching' WHERE id = $1",
        "UPDATE requests SET kind = 'book' WHERE id = $1",
        // Details are an object, refs an array.
        "UPDATE requests SET details = '[]' WHERE id = $1",
        "UPDATE requests SET refs = '{}' WHERE id = $1",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(id)
                .execute(&pool)
                .await
                .is_err(),
            "{statement}"
        );
    }
}
