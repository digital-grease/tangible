// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Restoring a copy of the database: the steps that are SQL (ADR 0026,
//! docs/plans/backups-ui.md, D4).
//!
//! A copy is never restored as it stands. Its definitions are never run:
//! the schema comes from this version's own migrations, in a new database
//! beside the one in use; only the copy's rows are loaded into it, by a
//! login role made for that restore alone, which may insert rows and set
//! sequences and nothing else. Whatever SQL a doctored copy carries in its
//! data entries runs as that role. Then the foreign keys are added again,
//! which checks every row against them, the rest of the migrations are
//! applied, and the two databases swap names.
//!
//! The database user the server connects as must be able to create
//! databases and roles: PostgreSQL's superuser in the Compose stack.

use std::str::FromStr as _;

use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{Connection as _, Executor as _, PgConnection, PgPool};
use tangible_domain::SecretString;
use time::OffsetDateTime;

use crate::DbError;

/// How to reach the PostgreSQL server the database is on: what libpq's
/// tools are given in their environment, and what these steps connect
/// with. The password never prints.
#[derive(Clone)]
pub struct ToolConnection {
    /// The host, or a directory holding PostgreSQL's socket.
    pub host: String,
    /// The port.
    pub port: u16,
    /// The database user.
    pub user: String,
    /// Its password, if it has one.
    pub password: Option<SecretString>,
    /// The database.
    pub database: String,
    /// libpq's `sslmode`, when the server's URL names one.
    pub sslmode: Option<String>,
}

impl std::fmt::Debug for ToolConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolConnection")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("database", &self.database)
            .field("sslmode", &self.sslmode)
            .finish_non_exhaustive()
    }
}

impl ToolConnection {
    /// libpq's environment for a run connecting as `user` to `database`.
    #[must_use]
    pub fn environment(
        &self,
        user: &str,
        password: Option<&SecretString>,
        database: &str,
    ) -> Vec<(&'static str, String)> {
        let mut env = vec![
            ("PGHOST", self.host.clone()),
            ("PGPORT", self.port.to_string()),
            ("PGUSER", user.to_owned()),
            ("PGDATABASE", database.to_owned()),
            ("PGAPPNAME", "tangible-backup".to_owned()),
            ("PGCONNECT_TIMEOUT", "10".to_owned()),
            // Times and messages the same wherever the server runs.
            ("PGTZ", "UTC".to_owned()),
            ("LC_ALL", "C".to_owned()),
        ];
        if let Some(password) = password {
            env.push(("PGPASSWORD", password.expose().to_owned()));
        }
        if let Some(mode) = &self.sslmode {
            env.push(("PGSSLMODE", mode.clone()));
        }
        env
    }

    /// Connection settings for `database`, as `user` with `password`.
    fn options(
        &self,
        user: &str,
        password: Option<&SecretString>,
        database: &str,
    ) -> PgConnectOptions {
        let mut options = PgConnectOptions::new()
            .host(&self.host)
            .port(self.port)
            .username(user)
            .database(database)
            .application_name("tangible-restore");
        if let Some(password) = password {
            options = options.password(password.expose());
        }
        if let Some(mode) = self
            .sslmode
            .as_deref()
            .and_then(|mode| PgSslMode::from_str(mode).ok())
        {
            options = options.ssl_mode(mode);
        }
        options
    }

    async fn connect(&self, database: &str) -> Result<PgConnection, DbError> {
        PgConnection::connect_with(&self.options(&self.user, self.password.as_ref(), database))
            .await
            .map_err(DbError::Connect)
    }
}

/// A pool on `database`, as the server's user: the restore check reads the
/// restored database through it before it is put in place.
///
/// # Errors
///
/// [`DbError::Connect`].
pub async fn open(connection: &ToolConnection, database: &str) -> Result<crate::Database, DbError> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(connection.options(&connection.user, connection.password.as_ref(), database))
        .await
        .map_err(DbError::Connect)?;
    Ok(crate::Database { pool })
}

/// The database a restore is made in, beside `database`.
#[must_use]
pub fn restoring_name(database: &str) -> String {
    format!("{database}_restoring")
}

/// The longest database name a restore can make its neighbour's name from,
/// within PostgreSQL's 63 bytes.
pub const MAX_DATABASE_NAME: usize = 40;

/// A database or role name, quoted as an identifier.
#[must_use]
pub fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Whether the user connected may restore this way: create databases and
/// roles, and rename and drop the database it owns.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn may_restore(pool: &PgPool) -> Result<bool, DbError> {
    sqlx::query_scalar(
        "SELECT rolsuper OR (rolcreatedb AND rolcreaterole)
         FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)
}

/// What is under way that a restore would cut off: by kind, state, how
/// many. Queued work is not counted; it is replaced with the rest.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn work_under_way(pool: &PgPool) -> Result<Vec<(&'static str, String, i64)>, DbError> {
    const UNDER_WAY: [(&str, &str, &str); 7] = [
        (
            "import",
            "import_jobs",
            "'acquiring', 'staged', 'hashing', 'inspecting', 'registering'",
        ),
        (
            "burn",
            "burn_jobs",
            "'leased', 'staging', 'waiting_for_media', 'preflighting', 'ready', 'writing', \
             'finalizing', 'verifying'",
        ),
        ("rip", "rip_jobs", "'reading', 'uploading', 'registering'"),
        ("erasure", "media_erasures", "'erasing'"),
        ("derivation", "derivation_jobs", "'running'"),
        ("integrity_check", "integrity_checks", "'running'"),
        (
            "source_removal",
            "source_removals",
            "'verifying', 'removing'",
        ),
    ];
    let mut found = Vec::new();
    for (kind, table, states) in UNDER_WAY {
        // The names and states are this function's own constants.
        let rows: Vec<(String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT state, count(*) FROM {table} WHERE state IN ({states}) \
             GROUP BY state ORDER BY state"
        )))
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)?;
        found.extend(rows.into_iter().map(|(state, count)| (kind, state, count)));
    }
    let collecting: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gc_plans
         WHERE collect_requested_at IS NOT NULL AND collected_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;
    if collecting > 0 {
        found.push(("storage_collection", "queued".to_owned(), collecting));
    }
    Ok(found)
}

/// Make the database a restore is made in, empty, dropping one a restore
/// that died left behind.
///
/// # Errors
///
/// [`DbError::Connect`] or [`DbError::Query`].
pub async fn create_restoring(connection: &ToolConnection) -> Result<String, DbError> {
    let name = restoring_name(&connection.database);
    let mut maintenance = connection.connect("postgres").await?;
    end_connections(&mut maintenance, &name).await?;
    run(
        &mut maintenance,
        &format!("DROP DATABASE IF EXISTS {}", quoted(&name)),
    )
    .await?;
    run(
        &mut maintenance,
        &format!("CREATE DATABASE {}", quoted(&name)),
    )
    .await?;
    maintenance.close().await.map_err(DbError::Query)?;
    Ok(name)
}

/// Apply this version's migrations to `database` up to `version`, the
/// newest the copy records: the schema it was taken from, and Tangible's
/// own.
///
/// # Errors
///
/// [`DbError::Connect`] or [`DbError::Migrate`].
pub async fn migrate_to(
    connection: &ToolConnection,
    database: &str,
    version: i64,
) -> Result<(), DbError> {
    let mut conn = connection.connect(database).await?;
    crate::MIGRATOR
        .run_to(version, &mut conn)
        .await
        .map_err(DbError::Migrate)?;
    conn.close().await.map_err(DbError::Query)
}

/// Apply the rest of this version's migrations to `database`.
///
/// # Errors
///
/// [`DbError::Connect`] or [`DbError::Migrate`].
pub async fn migrate_all(connection: &ToolConnection, database: &str) -> Result<(), DbError> {
    let mut conn = connection.connect(database).await?;
    crate::MIGRATOR
        .run(&mut conn)
        .await
        .map_err(DbError::Migrate)?;
    conn.close().await.map_err(DbError::Query)
}

/// A foreign key taken off while rows are loaded, to put back after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// Its table, as PostgreSQL names it.
    pub table: String,
    /// Its name.
    pub name: String,
    /// Its definition.
    pub definition: String,
}

/// The role a restore's rows are loaded as, and what was taken off to load
/// them.
#[derive(Debug)]
pub struct Loader {
    /// The role's name.
    pub role: String,
    /// Its password, made for this restore.
    pub password: SecretString,
    /// The tables a copy's rows may be loaded into.
    pub tables: Vec<String>,
    /// The sequences a copy may set.
    pub sequences: Vec<String>,
    foreign_keys: Vec<ForeignKey>,
}

/// Ready `database` for a copy's rows: its foreign keys taken off and kept,
/// its triggers off, and a role made that may insert rows into its tables
/// and set its sequences, and do nothing else.
///
/// # Errors
///
/// [`DbError::Connect`] or [`DbError::Query`].
pub async fn prepare_loader(
    connection: &ToolConnection,
    database: &str,
    password: SecretString,
) -> Result<Loader, DbError> {
    let mut conn = connection.connect(database).await?;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' AND c.relkind = 'r' AND c.relname <> '_sqlx_migrations'
         ORDER BY 1",
    )
    .fetch_all(&mut conn)
    .await
    .map_err(DbError::Query)?;
    let sequences: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' AND c.relkind = 'S'
         ORDER BY 1",
    )
    .fetch_all(&mut conn)
    .await
    .map_err(DbError::Query)?;
    let foreign_keys: Vec<ForeignKey> = sqlx::query_as::<_, (String, String, String)>(
        "SELECT c.conrelid::regclass::text, c.conname::text, pg_get_constraintdef(c.oid)
         FROM pg_constraint c
         JOIN pg_namespace n ON n.oid = c.connamespace
         WHERE n.nspname = 'public' AND c.contype = 'f'
         ORDER BY 1, 2",
    )
    .fetch_all(&mut conn)
    .await
    .map_err(DbError::Query)?
    .into_iter()
    .map(|(table, name, definition)| ForeignKey {
        table,
        name,
        definition,
    })
    .collect();

    let role = format!("tangible_restore_{}", uuid::Uuid::now_v7().simple());
    let mut tx = conn.begin().await.map_err(DbError::Query)?;
    for key in &foreign_keys {
        run(
            &mut tx,
            &format!(
                "ALTER TABLE {} DROP CONSTRAINT {}",
                key.table,
                quoted(&key.name)
            ),
        )
        .await?;
    }
    for table in &tables {
        run(
            &mut tx,
            &format!("ALTER TABLE public.{} DISABLE TRIGGER USER", quoted(table)),
        )
        .await?;
    }
    // A role's password cannot be a bound parameter; this one is hex.
    run(
        &mut tx,
        &format!(
            "CREATE ROLE {} LOGIN PASSWORD '{}' NOSUPERUSER NOCREATEDB NOCREATEROLE \
             NOINHERIT NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 2",
            quoted(&role),
            password.expose()
        ),
    )
    .await?;
    let db = quoted(database);
    let who = quoted(&role);
    for statement in [
        format!("REVOKE ALL ON DATABASE {db} FROM PUBLIC"),
        "REVOKE ALL ON SCHEMA public FROM PUBLIC".to_owned(),
        format!("GRANT CONNECT ON DATABASE {db} TO {who}"),
        format!("GRANT USAGE ON SCHEMA public TO {who}"),
        format!("GRANT INSERT ON ALL TABLES IN SCHEMA public TO {who}"),
        format!("GRANT UPDATE ON ALL SEQUENCES IN SCHEMA public TO {who}"),
    ] {
        run(&mut tx, &statement).await?;
    }
    tx.commit().await.map_err(DbError::Query)?;
    conn.close().await.map_err(DbError::Query)?;
    Ok(Loader {
        role,
        password,
        tables,
        sequences,
        foreign_keys,
    })
}

/// Undo [`prepare_loader`] once the rows are in: the role ended and
/// dropped, the triggers on, the foreign keys added again, which checks
/// every row against them, and the database's usual grants back.
///
/// # Errors
///
/// [`DbError::Connect`], or [`DbError::Query`] for a row a foreign key
/// refuses, which names the key.
pub async fn finish_loader(
    connection: &ToolConnection,
    database: &str,
    loader: &Loader,
) -> Result<(), DbError> {
    let mut conn = connection.connect(database).await?;
    drop_role(&mut conn, database, &loader.role).await?;
    let mut tx = conn.begin().await.map_err(DbError::Query)?;
    for table in &loader.tables {
        run(
            &mut tx,
            &format!("ALTER TABLE public.{} ENABLE TRIGGER USER", quoted(table)),
        )
        .await?;
    }
    for key in &loader.foreign_keys {
        run(
            &mut tx,
            &format!(
                "ALTER TABLE {} ADD CONSTRAINT {} {}",
                key.table,
                quoted(&key.name),
                key.definition
            ),
        )
        .await?;
    }
    let db = quoted(database);
    for statement in [
        format!("GRANT CONNECT, TEMPORARY ON DATABASE {db} TO PUBLIC"),
        "GRANT USAGE ON SCHEMA public TO PUBLIC".to_owned(),
    ] {
        run(&mut tx, &statement).await?;
    }
    tx.commit().await.map_err(DbError::Query)?;
    conn.close().await.map_err(DbError::Query)
}

/// End a restore role's connections, take back what it was granted in
/// `database`, and drop it.
async fn drop_role(conn: &mut PgConnection, database: &str, role: &str) -> Result<(), DbError> {
    sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = $1")
        .bind(role)
        .execute(&mut *conn)
        .await
        .map_err(DbError::Query)?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(role)
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    if !exists {
        return Ok(());
    }
    run(conn, &format!("DROP OWNED BY {}", quoted(role))).await?;
    run(
        conn,
        &format!(
            "REVOKE ALL ON DATABASE {} FROM {}",
            quoted(database),
            quoted(role)
        ),
    )
    .await?;
    run(conn, &format!("DROP ROLE {}", quoted(role))).await
}

/// Put things back after a restore that failed before the swap: the
/// restoring database dropped, and the role with it.
///
/// # Errors
///
/// [`DbError::Connect`] or [`DbError::Query`].
pub async fn abandon(connection: &ToolConnection, role: Option<&str>) -> Result<(), DbError> {
    let name = restoring_name(&connection.database);
    let mut maintenance = connection.connect("postgres").await?;
    if let Some(role) = role {
        // Its grants are in the database about to go; nothing else holds it.
        sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = $1")
            .bind(role)
            .execute(&mut maintenance)
            .await
            .map_err(DbError::Query)?;
    }
    end_connections(&mut maintenance, &name).await?;
    run(
        &mut maintenance,
        &format!("DROP DATABASE IF EXISTS {}", quoted(&name)),
    )
    .await?;
    if let Some(role) = role {
        run(
            &mut maintenance,
            &format!("DROP ROLE IF EXISTS {}", quoted(role)),
        )
        .await?;
    }
    maintenance.close().await.map_err(DbError::Query)
}

/// End every session in `database`: a sign-in the copy remembers, which
/// was ended since, must not come back.
///
/// # Errors
///
/// [`DbError::Connect`] or [`DbError::Query`].
pub async fn end_sessions(connection: &ToolConnection, database: &str) -> Result<u64, DbError> {
    let mut conn = connection.connect(database).await?;
    let ended = sqlx::query("DELETE FROM user_sessions")
        .execute(&mut conn)
        .await
        .map_err(DbError::Query)?
        .rows_affected();
    conn.close().await.map_err(DbError::Query)?;
    Ok(ended)
}

/// A restore, as the database it made records it, or the one it left.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct RestoreRecord {
    /// Its identifier.
    pub id: uuid::Uuid,
    /// Who asked.
    pub requested_by: String,
    /// The copy restored.
    pub copy_name: String,
    /// The copy taken of the database as it was, first.
    pub safety_copy: Option<String>,
    /// Whether it was restored; otherwise why not.
    pub outcome: String,
    /// Why it failed, when it did.
    pub error: Option<String>,
    /// The restore check's report, when it was restored.
    pub report: Option<serde_json::Value>,
    /// When it began.
    pub started_at: OffsetDateTime,
    /// When it ended.
    pub finished_at: OffsetDateTime,
}

/// Keep `record` in `database`, and audit it as `database.restored` (or
/// `database.restore_failed`).
///
/// # Errors
///
/// [`DbError::Connect`] or [`DbError::Query`].
pub async fn record(
    connection: &ToolConnection,
    database: &str,
    record: &RestoreRecord,
) -> Result<(), DbError> {
    let mut conn = connection.connect(database).await?;
    let mut tx = conn.begin().await.map_err(DbError::Query)?;
    sqlx::query(
        "INSERT INTO database_restores
             (id, requested_by, copy_name, safety_copy, outcome, error, report,
              started_at, finished_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(record.id)
    .bind(&record.requested_by)
    .bind(&record.copy_name)
    .bind(&record.safety_copy)
    .bind(&record.outcome)
    .bind(&record.error)
    .bind(&record.report)
    .bind(record.started_at)
    .bind(record.finished_at)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let (action, outcome) = if record.outcome == "restored" {
        ("database.restored", "success")
    } else {
        ("database.restore_failed", "failure")
    };
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, $3, 'backup', $4, $5, $6)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(&record.requested_by)
    .bind(action)
    .bind(&record.copy_name)
    .bind(outcome)
    .bind(serde_json::json!({
        "restore_id": record.id.to_string(),
        "safety_copy": record.safety_copy,
        "error": record.error,
    }))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    conn.close().await.map_err(DbError::Query)
}

/// The last restore this database records, if any.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn last_restore(pool: &PgPool) -> Result<Option<RestoreRecord>, DbError> {
    sqlx::query_as::<_, RestoreRecord>(
        "SELECT id, requested_by, copy_name, safety_copy, outcome, error, report,
                started_at, finished_at
         FROM database_restores ORDER BY finished_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)
}

/// Put the restored database in place of the one in use: every connection
/// to either ended, then, in one transaction, the one in use renamed aside
/// and the restored one given its name; then the one set aside dropped.
/// Returns whether that last step failed, which leaves it to remove.
///
/// # Errors
///
/// [`DbError::Connect`] or [`DbError::Query`]; nothing is renamed then.
pub async fn swap(connection: &ToolConnection) -> Result<Option<String>, DbError> {
    let live = connection.database.clone();
    let restoring = restoring_name(&live);
    let aside = format!("{live}_replaced");
    let mut maintenance = connection.connect("postgres").await?;
    run(
        &mut maintenance,
        &format!("DROP DATABASE IF EXISTS {}", quoted(&aside)),
    )
    .await?;
    let mut tries = 0;
    loop {
        end_connections(&mut maintenance, &live).await?;
        end_connections(&mut maintenance, &restoring).await?;
        let renamed: Result<(), DbError> = async {
            let mut tx = maintenance.begin().await.map_err(DbError::Query)?;
            run(
                &mut tx,
                &format!(
                    "ALTER DATABASE {} RENAME TO {}",
                    quoted(&live),
                    quoted(&aside)
                ),
            )
            .await?;
            run(
                &mut tx,
                &format!(
                    "ALTER DATABASE {} RENAME TO {}",
                    quoted(&restoring),
                    quoted(&live)
                ),
            )
            .await?;
            tx.commit().await.map_err(DbError::Query)
        }
        .await;
        match renamed {
            Ok(()) => break,
            // Something connected again between ending the connections and
            // renaming: end them again, a few times.
            Err(_) if tries < 5 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(error) => return Err(error),
        }
    }
    end_connections(&mut maintenance, &aside).await?;
    let left = match run(
        &mut maintenance,
        &format!("DROP DATABASE {}", quoted(&aside)),
    )
    .await
    {
        Ok(()) => None,
        Err(error) => {
            tracing::warn!(error = ?error, database = %aside, "the database replaced could not be dropped");
            Some(aside)
        }
    };
    maintenance.close().await.map_err(DbError::Query)?;
    Ok(left)
}

async fn end_connections(conn: &mut PgConnection, database: &str) -> Result<(), DbError> {
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
         WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(database)
    .execute(&mut *conn)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// Run a statement built here from this module's own names, never input.
async fn run(conn: &mut PgConnection, statement: &str) -> Result<(), DbError> {
    conn.execute(sqlx::AssertSqlSafe(statement.to_owned()))
        .await
        .map_err(DbError::Query)?;
    Ok(())
}

/// One migration a copy records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedMigration {
    /// Its version.
    pub version: i64,
    /// Whether it succeeded.
    pub success: bool,
    /// Its checksum.
    pub checksum: Vec<u8>,
}

/// What a copy's migrations say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyVersion {
    /// Taken from this version's schema up to `newest`, every migration to
    /// it as this version has it.
    Known {
        /// Its newest migration.
        newest: i64,
    },
    /// Taken by a later version: migrations this one does not have.
    Newer(Vec<i64>),
    /// Not a copy of a Tangible database this version knows.
    Foreign(String),
}

/// Whether a copy's recorded migrations are this version's: each one this
/// version has, checksum for checksum, none missing below its newest, none
/// failed.
#[must_use]
pub fn copy_version(recorded: &[RecordedMigration]) -> CopyVersion {
    let known: Vec<(i64, &[u8])> = crate::MIGRATOR
        .iter()
        .map(|migration| (migration.version, migration.checksum.as_ref()))
        .collect();
    if recorded.is_empty() {
        return CopyVersion::Foreign("it records no migrations".to_owned());
    }
    if let Some(failed) = recorded.iter().find(|m| !m.success) {
        return CopyVersion::Foreign(format!("it records migration {} as failed", failed.version));
    }
    let unknown: Vec<i64> = recorded
        .iter()
        .map(|m| m.version)
        .filter(|version| !known.iter().any(|(k, _)| k == version))
        .collect();
    if !unknown.is_empty() {
        return CopyVersion::Newer(unknown);
    }
    for migration in recorded {
        let ours = known
            .iter()
            .find(|(version, _)| *version == migration.version)
            .map(|(_, checksum)| *checksum);
        if ours != Some(migration.checksum.as_slice()) {
            return CopyVersion::Foreign(format!(
                "its migration {} is not this version's",
                migration.version
            ));
        }
    }
    let newest = recorded.iter().map(|m| m.version).max().unwrap_or(0);
    let missing: Vec<i64> = known
        .iter()
        .map(|(version, _)| *version)
        .filter(|version| *version <= newest && !recorded.iter().any(|m| m.version == *version))
        .collect();
    if !missing.is_empty() {
        return CopyVersion::Foreign(format!(
            "it lacks migrations {missing:?} below its newest, {newest}"
        ));
    }
    CopyVersion::Known { newest }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ours() -> Vec<RecordedMigration> {
        crate::MIGRATOR
            .iter()
            .map(|m| RecordedMigration {
                version: m.version,
                success: true,
                checksum: m.checksum.to_vec(),
            })
            .collect()
    }

    #[test]
    fn a_copy_of_this_version_or_an_earlier_one_is_known() {
        let all = ours();
        let newest = all.last().unwrap().version;
        assert_eq!(copy_version(&all), CopyVersion::Known { newest });
        let earlier = &all[..all.len() - 1];
        assert_eq!(
            copy_version(earlier),
            CopyVersion::Known {
                newest: earlier.last().unwrap().version
            }
        );
    }

    #[test]
    fn a_copy_from_a_later_version_or_another_schema_is_not() {
        let mut later = ours();
        later.push(RecordedMigration {
            version: 99_990_101_000_000,
            success: true,
            checksum: vec![1, 2, 3],
        });
        assert_eq!(
            copy_version(&later),
            CopyVersion::Newer(vec![99_990_101_000_000])
        );

        let mut doctored = ours();
        doctored[3].checksum = vec![0; 48];
        assert!(
            matches!(copy_version(&doctored), CopyVersion::Foreign(why) if why.contains("is not this version's"))
        );

        let mut gap = ours();
        gap.remove(2);
        assert!(matches!(copy_version(&gap), CopyVersion::Foreign(why) if why.contains("lacks")));

        let mut failed = ours();
        failed[1].success = false;
        assert!(
            matches!(copy_version(&failed), CopyVersion::Foreign(why) if why.contains("failed"))
        );

        assert!(matches!(copy_version(&[]), CopyVersion::Foreign(_)));
    }

    #[test]
    fn names_are_quoted_and_the_neighbour_named() {
        assert_eq!(quoted("tangible"), "\"tangible\"");
        assert_eq!(quoted("odd\"name"), "\"odd\"\"name\"");
        assert_eq!(restoring_name("tangible"), "tangible_restoring");
    }
}
