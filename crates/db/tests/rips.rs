// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Rips against a real PostgreSQL.
//!
//! Each test makes its own worker and drive, so they run in parallel against
//! the shared database. The point is the drive: one rip at a time on it, and
//! never a rip, a burn and an erase together.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::{Executor as _, PgPool};
use tangible_db::erasures::{claim_erasure, request_erasure};
use tangible_db::repositories::{ClaimOutcome, claim_next_burn_job};
use tangible_db::rips::{
    CancelOutcome, ReceivedFile, RequestOutcome, RipEvent, RipOptions, RipResult, WorkerOutcome,
    cancel_rip, claim_rip, complete_rip, get_rip, list_rip_events, record_rip_events,
    record_upload, request_rip, settle_rips,
};
use tangible_db::{Database, DbConfig};
use tangible_domain::{DriveId, ErasureMode, RipJobId, RipOriginal, RipState, WorkerId};
use time::OffsetDateTime;

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

/// A worker with one drive.
async fn worker_and_drive(pool: &PgPool) -> (WorkerId, DriveId) {
    let worker = WorkerId::generate();
    let drive = DriveId::generate();
    for statement in [
        format!(
            "INSERT INTO workers (id, name, software_version, protocol_version, credential_hash) \
             VALUES ('{worker}', 'rip-{worker}', '0.1.0', '1alpha1', 'hash-{worker}')"
        ),
        format!(
            "INSERT INTO drives (id, worker_id, configured_name, device_alias) \
             VALUES ('{drive}', '{worker}', 'd1', '/dev/sr0')"
        ),
    ] {
        pool.execute(statement.as_str()).await.expect("seed");
    }
    (worker, drive)
}

/// A rip requested and taken, reading.
async fn reading(pool: &PgPool) -> (WorkerId, DriveId, RipJobId) {
    let (worker, drive) = worker_and_drive(pool).await;
    let rip = request_rip(pool, drive, "operator", RipOptions::default())
        .await
        .unwrap()
        .unwrap();
    let claimed = claim_rip(pool, worker, drive, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.map(|r| r.id), Some(rip.id));
    (worker, drive, rip.id)
}

async fn upload(pool: &PgPool, worker: WorkerId, rip: RipJobId, name: &str, bytes: i64) {
    record_upload(
        pool,
        worker,
        rip,
        &ReceivedFile {
            name,
            sha256: &"ab".repeat(32),
            bytes,
        },
    )
    .await
    .unwrap()
    .unwrap();
}

fn read_report(outputs: &[String]) -> RipResult<'_> {
    RipResult::Read {
        engine: "cdrdao",
        engine_version: "1.2.4",
        medium: None,
        read_errors: None,
        outputs,
        copy_outputs: &[],
        encryption: "none",
        decryption: None,
        decrypted_original: false,
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn one_rip_at_a_time_per_drive() {
    let pool = pool().await;
    let (_, drive) = worker_and_drive(&pool).await;
    let first = request_rip(&pool, drive, "operator", RipOptions::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.state, RipState::Queued);
    assert_eq!(first.import_job_id, None);
    assert_eq!(
        request_rip(&pool, drive, "operator", RipOptions::default())
            .await
            .unwrap(),
        Err(RequestOutcome::AlreadyOpen)
    );
    assert_eq!(
        request_rip(
            &pool,
            DriveId::generate(),
            "operator",
            RipOptions::default()
        )
        .await
        .unwrap(),
        Err(RequestOutcome::DriveMissing)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_the_drives_own_worker_takes_its_rip() {
    let pool = pool().await;
    let (_, drive) = worker_and_drive(&pool).await;
    let (stranger, _) = worker_and_drive(&pool).await;
    request_rip(&pool, drive, "operator", RipOptions::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim_rip(&pool, stranger, drive, false).await.unwrap(),
        Err(WorkerOutcome::NotYours)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_ripping_drive_neither_burns_nor_erases() {
    let pool = pool().await;
    let (worker, drive, _) = reading(&pool).await;

    let burn = claim_next_burn_job(&pool, worker, drive, "fake", "0.1.0", "lease-x", 90)
        .await
        .unwrap();
    assert_eq!(burn.err(), Some(ClaimOutcome::DriveBusy));

    request_erasure(&pool, drive, ErasureMode::Quick, "admin")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim_erasure(&pool, worker, drive).await.unwrap(), Ok(None));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_erasing_drive_does_not_rip() {
    let pool = pool().await;
    let (worker, drive) = worker_and_drive(&pool).await;
    request_erasure(&pool, drive, ErasureMode::Quick, "admin")
        .await
        .unwrap()
        .unwrap();
    claim_erasure(&pool, worker, drive).await.unwrap().unwrap();
    request_rip(&pool, drive, "operator", RipOptions::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim_rip(&pool, worker, drive, false).await.unwrap(),
        Ok(None)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_rip_abandoned_by_a_restart_is_closed_when_the_worker_asks_again() {
    let pool = pool().await;
    let (worker, drive, rip) = reading(&pool).await;
    assert_eq!(
        claim_rip(&pool, worker, drive, false).await.unwrap(),
        Ok(None)
    );
    let closed = get_rip(&pool, rip).await.unwrap().unwrap();
    assert_eq!(closed.state, RipState::Failed);
    assert_eq!(closed.error_code.as_deref(), Some("WORKER_RESTARTED"));
    assert!(closed.completed_at.is_some());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_read_rip_queues_its_import_with_the_staging_areas_id() {
    let pool = pool().await;
    let (worker, _, rip) = reading(&pool).await;
    upload(&pool, worker, rip, "disc.toc", 300).await;
    let after = get_rip(&pool, rip).await.unwrap().unwrap();
    assert_eq!(after.state, RipState::Uploading);
    upload(&pool, worker, rip, "disc.bin", 23_520).await;
    // Sending a file again replaces it.
    upload(&pool, worker, rip, "disc.bin", 23_520).await;
    assert_eq!(
        get_rip(&pool, rip).await.unwrap().unwrap().bytes_received,
        23_820
    );

    let outputs = vec!["disc.toc".to_owned(), "disc.bin".to_owned()];
    let done = complete_rip(&pool, worker, rip, &read_report(&outputs))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.state, RipState::Registering);
    assert_eq!(done.import_job_id, Some(done.staging_id));
    assert_eq!(done.engine.as_deref(), Some("cdrdao"));

    let (kind, state, bytes): (String, String, i64) =
        sqlx::query_as("SELECT source_kind, state, bytes_received FROM import_jobs WHERE id = $1")
            .bind(done.staging_id.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (kind.as_str(), state.as_str(), bytes),
        ("physical_drive", "staged", 23_820)
    );

    // Reported again after a lost response: accepted, nothing new.
    let again = complete_rip(&pool, worker, rip, &read_report(&outputs))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.state, RipState::Registering);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_files_named_must_be_exactly_the_files_that_arrived() {
    let pool = pool().await;
    let (worker, _, rip) = reading(&pool).await;
    upload(&pool, worker, rip, "disc.toc", 300).await;
    for named in [vec!["disc.toc".to_owned(), "disc.bin".to_owned()], vec![]] {
        assert_eq!(
            complete_rip(&pool, worker, rip, &read_report(&named))
                .await
                .unwrap(),
            Err(WorkerOutcome::MissingOutputs)
        );
    }
    assert_eq!(
        get_rip(&pool, rip).await.unwrap().unwrap().state,
        RipState::Uploading
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn nothing_read_cannot_be_registered() {
    let pool = pool().await;
    let (worker, _, rip) = reading(&pool).await;
    let outputs = vec!["disc.toc".to_owned()];
    assert_eq!(
        complete_rip(&pool, worker, rip, &read_report(&outputs))
            .await
            .unwrap(),
        Err(WorkerOutcome::Conflict(RipState::Reading))
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_refuses_what_is_not_there() {
    let pool = pool().await;
    let (worker, drive, rip) = reading(&pool).await;
    let medium = serde_json::json!({ "profile": "none" });
    let refused = complete_rip(
        &pool,
        worker,
        rip,
        &RipResult::Refused {
            code: "NO_MEDIUM",
            detail: Some("the drive is empty"),
            medium: Some(&medium),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(refused.state, RipState::Refused);
    assert_eq!(refused.error_code.as_deref(), Some("NO_MEDIUM"));
    assert_eq!(refused.medium, Some(medium));
    // The drive is free for the next request.
    request_rip(&pool, drive, "operator", RipOptions::default())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_rip_can_be_withdrawn_until_its_import_starts() {
    let pool = pool().await;
    let (worker, _, rip) = reading(&pool).await;
    upload(&pool, worker, rip, "disc.toc", 300).await;
    let canceled = cancel_rip(&pool, rip, "operator").await.unwrap().unwrap();
    assert_eq!(canceled.state, RipState::Canceled);
    // The worker learns of it the next time it reports.
    let event = RipEvent {
        sequence: 0,
        event_type: "progress",
        message_code: "READING",
        progress_fraction: Some(0.5),
        data: &serde_json::json!({}),
        worker_timestamp: OffsetDateTime::now_utc(),
    };
    assert_eq!(
        record_rip_events(&pool, worker, rip, &[event])
            .await
            .unwrap(),
        Err(WorkerOutcome::Conflict(RipState::Canceled))
    );

    let (worker, _, registering) = reading(&pool).await;
    upload(&pool, worker, registering, "disc.toc", 1).await;
    let outputs = vec!["disc.toc".to_owned()];
    complete_rip(&pool, worker, registering, &read_report(&outputs))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cancel_rip(&pool, registering, "operator").await.unwrap(),
        Err(CancelOutcome::TooLate(RipState::Registering))
    );
    assert_eq!(
        cancel_rip(&pool, RipJobId::generate(), "operator")
            .await
            .unwrap(),
        Err(CancelOutcome::NotFound)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn events_are_idempotent_by_sequence() {
    let pool = pool().await;
    let (worker, _, rip) = reading(&pool).await;
    let data = serde_json::json!({ "bytes": 100 });
    let event = |sequence| RipEvent {
        sequence,
        event_type: "progress",
        message_code: "READING",
        progress_fraction: Some(0.1),
        data: &data,
        worker_timestamp: OffsetDateTime::now_utc(),
    };
    for _ in 0..2 {
        assert_eq!(
            record_rip_events(&pool, worker, rip, &[event(0), event(1)])
                .await
                .unwrap(),
            Ok(RipState::Reading)
        );
    }
    let stored = list_rip_events(&pool, rip, None, 100).await.unwrap();
    assert_eq!(
        stored.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(
        list_rip_events(&pool, rip, Some(0), 100)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_rip_ends_with_its_import() {
    let pool = pool().await;
    let outputs = vec!["disc.toc".to_owned()];

    // One whose import completes.
    let (worker, _, done) = reading(&pool).await;
    upload(&pool, worker, done, "disc.toc", 1).await;
    let record = complete_rip(&pool, worker, done, &read_report(&outputs))
        .await
        .unwrap()
        .unwrap();
    let artifact = uuid::Uuid::now_v7();
    let digest = uuid::Uuid::now_v7().simple().to_string().repeat(2);
    for statement in [
        format!("INSERT INTO cas_objects (sha256, size_bytes) VALUES ('{digest}', 1)"),
        format!(
            "INSERT INTO artifacts (id, origin, manifest_version, total_bytes, component_count, \
             validation_state) VALUES ('{artifact}', 'physical_dump', 'v1alpha1', 1, 1, 'valid')"
        ),
        format!(
            "UPDATE import_jobs SET state = 'complete', artifact_id = '{artifact}', \
             completed_at = now() WHERE id = '{}'",
            record.staging_id
        ),
    ] {
        pool.execute(statement.as_str())
            .await
            .expect("finish the import");
    }

    // One whose import fails for good.
    let (worker, _, lost) = reading(&pool).await;
    upload(&pool, worker, lost, "disc.toc", 1).await;
    let failing = complete_rip(&pool, worker, lost, &read_report(&outputs))
        .await
        .unwrap()
        .unwrap();
    pool.execute(
        format!(
            "UPDATE import_jobs SET state = 'failed_terminal', error_code = 'IMPORT_READ_FAILED', \
             completed_at = now() WHERE id = '{}'",
            failing.staging_id
        )
        .as_str(),
    )
    .await
    .expect("fail the import");

    let moved = settle_rips(&pool).await.unwrap();
    assert!(moved.contains(&done) && moved.contains(&lost), "{moved:?}");
    let done = get_rip(&pool, done).await.unwrap().unwrap();
    assert_eq!(done.state, RipState::Complete);
    assert_eq!(done.artifact_id, Some(artifact));
    let lost = get_rip(&pool, lost).await.unwrap().unwrap();
    assert_eq!(lost.state, RipState::Failed);
    assert_eq!(lost.error_code.as_deref(), Some("IMPORT_FAILED"));
    assert!(
        lost.error_detail
            .as_deref()
            .unwrap()
            .contains("IMPORT_READ_FAILED")
    );
}

/// Say the worker can decrypt DVDs, as a worker from the opt-in image does.
async fn can_decrypt(pool: &PgPool, worker: WorkerId) {
    sqlx::query(
        "UPDATE workers SET capabilities_json = jsonb_build_object('rip', jsonb_build_object(
             'engines', jsonb_build_array(jsonb_build_object('name', 'block_reader', 'version', '0.1.0')),
             'decryption', jsonb_build_object('css', jsonb_build_object(
                 'library', 'libdvdcss', 'version', '1.6.0',
                 'tool', 'dvdbackup', 'tool_version', '0.4.2-4.1'))))
         WHERE id = $1",
    )
    .bind(worker.as_uuid())
    .execute(pool)
    .await
    .expect("set capabilities");
}

const DECRYPT: RipOptions = RipOptions {
    decrypt: true,
    keep: RipOriginal::DecryptedOriginal,
};

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_choice_of_original_is_kept_only_when_decrypting() {
    let pool = pool().await;
    let (_, drive) = worker_and_drive(&pool).await;
    let plain = request_rip(
        &pool,
        drive,
        "operator",
        RipOptions {
            decrypt: false,
            keep: RipOriginal::DecryptedOriginal,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!plain.decrypt);
    assert_eq!(plain.keep, None);
    cancel_rip(&pool, plain.id, "operator")
        .await
        .unwrap()
        .unwrap();

    let decrypting = request_rip(&pool, drive, "operator", DECRYPT)
        .await
        .unwrap()
        .unwrap();
    assert!(decrypting.decrypt);
    assert_eq!(decrypting.keep, Some(RipOriginal::DecryptedOriginal));

    // The database holds the same rule.
    for (decrypt, keep) in [(false, Some("encrypted_original")), (true, None)] {
        let error = sqlx::query(
            "INSERT INTO rip_jobs (id, drive_id, requested_by, staging_id, decrypt, keep, state,
                                   completed_at)
             VALUES ($1, $2, 'operator', $3, $4, $5, 'canceled', now())",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(drive.as_uuid())
        .bind(uuid::Uuid::now_v7())
        .bind(decrypt)
        .bind(keep)
        .execute(&pool)
        .await
        .expect_err("refused");
        assert!(
            error
                .to_string()
                .contains("rip_jobs_keep_only_when_decrypting"),
            "{error}"
        );
    }
    let error = sqlx::query("UPDATE rip_jobs SET keep = 'scrambled' WHERE id = $1")
        .bind(decrypting.id.as_uuid())
        .execute(&pool)
        .await
        .expect_err("refused");
    assert!(error.to_string().contains("check"), "{error}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_decrypting_rip_goes_only_to_a_worker_that_can_on_a_server_that_allows() {
    let pool = pool().await;

    // The server does not allow it: refused as the worker asks, nothing read.
    let (worker, drive) = worker_and_drive(&pool).await;
    can_decrypt(&pool, worker).await;
    let rip = request_rip(&pool, drive, "operator", DECRYPT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim_rip(&pool, worker, drive, false).await.unwrap(),
        Ok(None)
    );
    let refused = get_rip(&pool, rip.id).await.unwrap().unwrap();
    assert_eq!(refused.state, RipState::Refused);
    assert_eq!(refused.error_code.as_deref(), Some("DECRYPTION_OFF"));
    assert!(refused.started_at.is_none());
    assert!(refused.completed_at.is_some());

    // The worker cannot: refused too.
    let (worker, drive) = worker_and_drive(&pool).await;
    let rip = request_rip(&pool, drive, "operator", DECRYPT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim_rip(&pool, worker, drive, true).await.unwrap(),
        Ok(None)
    );
    let refused = get_rip(&pool, rip.id).await.unwrap().unwrap();
    assert_eq!(refused.state, RipState::Refused);
    assert_eq!(
        refused.error_code.as_deref(),
        Some("ENCRYPTED_NO_DECRYPTION")
    );
    // The drive is free again: the next rip, without decryption, is taken.
    let plain = request_rip(&pool, drive, "operator", RipOptions::default())
        .await
        .unwrap()
        .unwrap();
    let taken = claim_rip(&pool, worker, drive, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.map(|r| r.id), Some(plain.id));

    // Both: taken, with the choice.
    let (worker, drive) = worker_and_drive(&pool).await;
    can_decrypt(&pool, worker).await;
    let rip = request_rip(&pool, drive, "operator", DECRYPT)
        .await
        .unwrap()
        .unwrap();
    let taken = claim_rip(&pool, worker, drive, true)
        .await
        .unwrap()
        .unwrap()
        .expect("taken");
    assert_eq!(taken.id, rip.id);
    assert_eq!(taken.state, RipState::Reading);
    assert!(taken.decrypt);
    assert_eq!(taken.keep, Some(RipOriginal::DecryptedOriginal));
}
