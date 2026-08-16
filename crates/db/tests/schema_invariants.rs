// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The schema constraints, exercised against a real PostgreSQL.
//!
//! These are `#[ignore]`d so `cargo test` stays hardware- and
//! database-free; CI's integration job and `just test-integration` run them
//! with `--ignored`.
//!
//! Each test asserts a *rejection*. A constraint that never rejects anything
//! is indistinguishable from a comment, and several of these protect against
//! outcomes measured in destroyed physical discs rather than bad rows.
//!
//! Every test runs inside a transaction that is rolled back, so they neither
//! interfere with one another nor need cleanup.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::{Executor, PgPool, Postgres, Transaction};
use tangible_db::{Database, DbConfig};
use uuid::Uuid;

/// Connect using the integration database URL.
async fn pool() -> PgPool {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL or TANGIBLE_DATABASE_URL to run integration tests");

    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect to the integration database");
    database
        .migrate()
        .await
        .expect("apply migrations to the integration database");
    database.pool().clone()
}

/// One test's fixture identifiers.
///
/// Generated per test rather than fixed: the tests run in parallel, and
/// uncommitted inserts of the same primary key block one another even when
/// every test rolls back. Generated ids also make the suite immune to rows
/// left behind by any earlier manual poking at the database.
struct Fixture {
    title: Uuid,
    edition: Uuid,
    disc_set: Uuid,
    disc: Uuid,
    artifact: Uuid,
    component: Uuid,
    worker: Uuid,
    drive: Uuid,
    job_one: Uuid,
    job_two: Uuid,
    attempt: Uuid,
    digest_a: String,
    digest_b: String,
}

impl Fixture {
    fn new() -> Self {
        // Digests must be 64 lowercase hex characters and unique per test, so
        // derive them from a fresh uuid rather than using a constant.
        let hex = |seed: Uuid| seed.simple().to_string().repeat(2);
        Self {
            title: Uuid::now_v7(),
            edition: Uuid::now_v7(),
            disc_set: Uuid::now_v7(),
            disc: Uuid::now_v7(),
            artifact: Uuid::now_v7(),
            component: Uuid::now_v7(),
            worker: Uuid::now_v7(),
            drive: Uuid::now_v7(),
            job_one: Uuid::now_v7(),
            job_two: Uuid::now_v7(),
            attempt: Uuid::now_v7(),
            digest_a: hex(Uuid::now_v7()),
            digest_b: hex(Uuid::now_v7()),
        }
    }
}

/// Seed one complete catalog-to-attempt chain inside the transaction.
async fn seed(tx: &mut Transaction<'_, Postgres>, f: &Fixture) {
    let statements = [
        format!(
            "INSERT INTO cas_objects (sha256, size_bytes) VALUES ('{}', 100), ('{}', 200)",
            f.digest_a, f.digest_b
        ),
        format!(
            "INSERT INTO titles (id, display_title, sort_title) VALUES ('{}', 'T', 'T')",
            f.title
        ),
        format!(
            "INSERT INTO editions (id, title_id, display_name) VALUES ('{}', '{}', 'E')",
            f.edition, f.title
        ),
        format!(
            "INSERT INTO disc_sets (id, edition_id, name) VALUES ('{}', '{}', 'S')",
            f.disc_set, f.edition
        ),
        format!(
            "INSERT INTO discs (id, disc_set_id, sequence_number) VALUES ('{}', '{}', 1)",
            f.disc, f.disc_set
        ),
        format!(
            "INSERT INTO artifacts (id, origin, manifest_version, total_bytes, component_count, validation_state) \
             VALUES ('{}', 'imported_original', 'v1alpha1', 100, 1, 'valid')",
            f.artifact
        ),
        format!(
            "INSERT INTO artifact_components (id, artifact_id, logical_path, length_bytes, sha256, ordinal) \
             VALUES ('{}', '{}', 'disc.iso', 100, '{}', 0)",
            f.component, f.artifact, f.digest_a
        ),
        format!(
            "INSERT INTO workers (id, name, software_version, protocol_version, credential_hash) \
             VALUES ('{}', 'worker-{}', '0.1.0', '1alpha1', 'hash')",
            f.worker, f.worker
        ),
        format!(
            "INSERT INTO drives (id, worker_id, configured_name, device_alias) \
             VALUES ('{}', '{}', 'd1', '/dev/disc-block')",
            f.drive, f.worker
        ),
        format!(
            "INSERT INTO burn_jobs (id, disc_id, artifact_id, created_by) VALUES ('{}', '{}', '{}', 'u')",
            f.job_one, f.disc, f.artifact
        ),
        format!(
            "INSERT INTO burn_jobs (id, disc_id, artifact_id, created_by) VALUES ('{}', '{}', '{}', 'u')",
            f.job_two, f.disc, f.artifact
        ),
        format!(
            "INSERT INTO burn_attempts \
             (id, burn_job_id, worker_id, drive_id, attempt_number, lease_token_hash, lease_expires_at, engine, engine_version, plan_json) \
             VALUES ('{}', '{}', '{}', '{}', 1, 'h', now() + interval '1 hour', 'fake', '0.1.0', '{{}}')",
            f.attempt, f.job_one, f.worker, f.drive
        ),
    ];
    for statement in statements {
        tx.execute(statement.as_str())
            .await
            .unwrap_or_else(|error| panic!("seeding failed on {statement}: {error}"));
    }
}

/// Assert that a statement is rejected, and report clearly when it is not.
///
/// A savepoint wraps the attempt: PostgreSQL aborts the whole transaction on
/// error, so without one the first rejection would poison every later query.
async fn must_reject(tx: &mut Transaction<'_, Postgres>, statement: &str, why: &str) {
    tx.execute("SAVEPOINT probe").await.expect("savepoint");
    let result = tx.execute(statement).await;
    assert!(
        result.is_err(),
        "the database accepted a statement it must reject: {why}\n  statement: {statement}"
    );
    tx.execute("ROLLBACK TO SAVEPOINT probe")
        .await
        .expect("rollback to savepoint");
}

/// Declare one rejection test.
///
/// The statement is a closure over the fixture rather than a bare expression:
/// macro hygiene means an identifier the macro introduces is invisible to
/// tokens passed in, so the caller has to name the binding itself.
macro_rules! invariant_test {
    ($name:ident, $why:literal, $build_statement:expr) => {
        #[tokio::test]
        #[ignore = "requires PostgreSQL"]
        async fn $name() {
            let pool = pool().await;
            let fixture = Fixture::new();
            let mut tx = pool.begin().await.expect("begin");
            seed(&mut tx, &fixture).await;
            let build: fn(&Fixture) -> String = $build_statement;
            let statement = build(&fixture);
            must_reject(&mut tx, &statement, $why).await;
            tx.rollback().await.expect("rollback");
        }
    };
}

// --- artifact immutability ---------------------------------------------------

invariant_test!(
    components_are_immutable_after_registration,
    "an artifact is a fixed set of bytes; mutating a component would silently \
     invalidate every hash, manifest and burn that referenced it",
    |f: &Fixture| format!(
        "UPDATE artifact_components SET length_bytes = 1 WHERE id = '{}'",
        f.component
    )
);

invariant_test!(
    logical_path_is_unique_within_an_artifact,
    "two components claiming one path would make materialization order-dependent",
    |f: &Fixture| format!(
        "INSERT INTO artifact_components (id, artifact_id, logical_path, length_bytes, sha256, ordinal) \
         VALUES (gen_random_uuid(), '{}', 'disc.iso', 1, '{}', 1)",
        f.artifact, f.digest_b
    )
);

invariant_test!(
    ordinal_is_unique_within_an_artifact,
    "component order must be total",
    |f: &Fixture| format!(
        "INSERT INTO artifact_components (id, artifact_id, logical_path, length_bytes, sha256, ordinal) \
         VALUES (gen_random_uuid(), '{}', 'other.iso', 1, '{}', 0)",
        f.artifact, f.digest_b
    )
);

// --- the path boundary, enforced a second time in SQL ------------------------

invariant_test!(
    logical_path_rejects_parent_traversal,
    "a descriptor must not be able to escape the staging root",
    |f: &Fixture| format!(
        "INSERT INTO artifact_components (id, artifact_id, logical_path, length_bytes, sha256, ordinal) \
         VALUES (gen_random_uuid(), '{}', '../escape.iso', 1, '{}', 9)",
        f.artifact, f.digest_b
    )
);

invariant_test!(
    logical_path_rejects_absolute_paths,
    "an absolute path in a descriptor must never be honoured",
    |f: &Fixture| format!(
        "INSERT INTO artifact_components (id, artifact_id, logical_path, length_bytes, sha256, ordinal) \
         VALUES (gen_random_uuid(), '{}', '/etc/passwd', 1, '{}', 10)",
        f.artifact, f.digest_b
    )
);

// --- storage -----------------------------------------------------------------

invariant_test!(
    referenced_cas_objects_cannot_be_deleted,
    "bytes outlive catalog edits",
    |f: &Fixture| format!("DELETE FROM cas_objects WHERE sha256 = '{}'", f.digest_a)
);

// --- catalog -----------------------------------------------------------------

invariant_test!(
    disc_sequence_is_unique_within_a_set,
    "two discs cannot both be disc 1 of the same set",
    |f: &Fixture| format!(
        "INSERT INTO discs (id, disc_set_id, sequence_number) VALUES (gen_random_uuid(), '{}', 1)",
        f.disc_set
    )
);

invariant_test!(
    a_compatibility_claim_requires_evidence,
    "the project must not imply a burned disc will satisfy console \
     authentication without something to back it",
    |f: &Fixture| format!(
        "INSERT INTO discs (id, disc_set_id, sequence_number, compatibility_claim) \
         VALUES (gen_random_uuid(), '{}', 99, 'player_compatibility_expected')",
        f.disc_set
    )
);

// --- burning: the constraints whose failure destroys physical media ----------

invariant_test!(
    only_one_active_attempt_per_drive,
    "two concurrent writes to one drive ruin the disc, so this is enforced by \
     the database rather than by application care",
    |f: &Fixture| format!(
        "INSERT INTO burn_attempts \
         (id, burn_job_id, worker_id, drive_id, attempt_number, lease_token_hash, lease_expires_at, engine, engine_version, plan_json) \
         VALUES (gen_random_uuid(), '{}', '{}', '{}', 1, 'h', now() + interval '1 hour', 'fake', '0.1.0', '{{}}')",
        f.job_two, f.worker, f.drive
    )
);

invariant_test!(
    no_physical_copy_without_a_completed_write,
    "recording a disc that was never burned would put phantom media in the \
     inventory",
    |f: &Fixture| format!(
        "INSERT INTO physical_copies (id, disc_id, artifact_id, burn_attempt_id, media_profile) \
         VALUES (gen_random_uuid(), '{}', '{}', '{}', 'CD-R')",
        f.disc, f.artifact, f.attempt
    )
);

invariant_test!(
    a_burn_job_cannot_skip_verification_entirely,
    "there is no skip-verification path in the main burn flow. Note this \
     needs cardinality(), not array_length(), which returns NULL for an \
     empty array and would let '{}' through a CHECK",
    |f: &Fixture| format!(
        "INSERT INTO burn_jobs (id, disc_id, artifact_id, created_by, verification_policy) \
         VALUES (gen_random_uuid(), '{}', '{}', 'u', '{{}}')",
        f.disc, f.artifact
    )
);

invariant_test!(
    enum_values_are_constrained_in_the_database,
    "application validation is not the only guard",
    |_f: &Fixture| {
        "INSERT INTO artifacts (id, origin, manifest_version, total_bytes, component_count) \
     VALUES (gen_random_uuid(), 'not_a_real_origin', 'v1', 0, 0)"
            .to_owned()
    }
);

// --- the positive cases, which matter just as much ---------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_failed_verification_still_records_its_ruined_disc() {
    // The disc physically exists and holds the wrong bytes. Refusing to record
    // it is how bad media gets shelved and reused.
    let pool = pool().await;
    let f = Fixture::new();
    let mut tx = pool.begin().await.expect("begin");
    seed(&mut tx, &f).await;

    tx.execute(
        format!(
            "UPDATE burn_attempts SET state = 'verification_failed' WHERE id = '{}'",
            f.attempt
        )
        .as_str(),
    )
    .await
    .expect("mark the attempt as verification-failed");

    tx.execute(
        format!(
            "INSERT INTO physical_copies (id, disc_id, artifact_id, burn_attempt_id, media_profile, status) \
             VALUES (gen_random_uuid(), '{}', '{}', '{}', 'CD-R', 'verification_failed')",
            f.disc, f.artifact, f.attempt
        )
        .as_str(),
    )
    .await
    .expect("a disc that was written but failed verification must be recordable");

    tx.rollback().await.expect("rollback");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_drive_frees_up_once_its_attempt_is_terminal() {
    // The partial unique index must not wedge a drive forever after a failure.
    let pool = pool().await;
    let f = Fixture::new();
    let mut tx = pool.begin().await.expect("begin");
    seed(&mut tx, &f).await;

    tx.execute(
        format!(
            "UPDATE burn_attempts SET state = 'write_failed' WHERE id = '{}'",
            f.attempt
        )
        .as_str(),
    )
    .await
    .expect("mark the attempt as failed");

    tx.execute(
        format!(
            "INSERT INTO burn_attempts \
             (id, burn_job_id, worker_id, drive_id, attempt_number, lease_token_hash, lease_expires_at, engine, engine_version, plan_json) \
             VALUES (gen_random_uuid(), '{}', '{}', '{}', 1, 'h', now() + interval '1 hour', 'fake', '0.1.0', '{{}}')",
            f.job_two, f.worker, f.drive
        )
        .as_str(),
    )
    .await
    .expect("the drive must accept a new attempt once the previous one is terminal");

    tx.rollback().await.expect("rollback");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn migrations_are_idempotent() {
    // A redeploy must not fail because migrations already ran.
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set a test database URL");
    for _ in 0..2 {
        let database = Database::connect(&DbConfig::new(url.clone()))
            .await
            .expect("connect");
        database
            .migrate()
            .await
            .expect("migrations must be re-runnable");
    }
}
