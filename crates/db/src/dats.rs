// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Preservation DATs and what they identify (docs/plans/dat-matching.md H2):
//! loading one (replacing one of the same name), listing and removing them,
//! and matching artifacts' preservation hashes against their files.
//!
//! An artifact's units are its CD tracks, when it has track hashes, and
//! otherwise its image files (not a descriptor, metadata or subchannel). A game matches when its
//! files other than a CUE are units of the artifact by SHA-1 and size:
//! `exact` when every one is and the artifact has no units left over,
//! `partial` when some are.

use std::collections::BTreeMap;

use sqlx::{PgPool, Postgres, Row, Transaction};
use time::OffsetDateTime;

use crate::DbError;

/// Rows sent to PostgreSQL at a time when loading.
const BATCH: usize = 5000;

/// One file of a game, to load.
#[derive(Debug, Clone, Copy)]
pub struct NewRom<'a> {
    /// Its name.
    pub name: &'a str,
    /// Its size.
    pub size: Option<u64>,
    /// CRC32.
    pub crc32: Option<&'a str>,
    /// MD5.
    pub md5: Option<&'a str>,
    /// SHA-1.
    pub sha1: Option<&'a str>,
    /// SHA-256.
    pub sha256: Option<&'a str>,
    /// Its status.
    pub status: Option<&'a str>,
    /// Whether it is a CUE sheet.
    pub is_cue: bool,
}

/// One game, to load.
#[derive(Debug, Clone)]
pub struct NewGame<'a> {
    /// Its name.
    pub name: &'a str,
    /// Its description.
    pub description: Option<&'a str>,
    /// Its category.
    pub category: Option<&'a str>,
    /// Its files.
    pub roms: Vec<NewRom<'a>>,
}

/// A DAT, to load.
#[derive(Debug, Clone)]
pub struct NewDat<'a> {
    /// Its header name, which identifies it.
    pub name: &'a str,
    /// Its description.
    pub description: Option<&'a str>,
    /// Its version.
    pub version: Option<&'a str>,
    /// Its author.
    pub author: Option<&'a str>,
    /// Its homepage.
    pub homepage: Option<&'a str>,
    /// Its address.
    pub url: Option<&'a str>,
    /// The SHA-256 of the file loaded.
    pub source_sha256: &'a str,
    /// Who loaded it.
    pub loaded_by: &'a str,
    /// Its games.
    pub games: Vec<NewGame<'a>>,
}

/// A DAT as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatRecord {
    /// Its id.
    pub id: uuid::Uuid,
    /// Its name.
    pub name: String,
    /// Its description.
    pub description: Option<String>,
    /// Its version.
    pub version: Option<String>,
    /// Its author.
    pub author: Option<String>,
    /// Its homepage.
    pub homepage: Option<String>,
    /// The SHA-256 of the file loaded.
    pub source_sha256: String,
    /// How many games.
    pub game_count: i32,
    /// How many files.
    pub rom_count: i32,
    /// Who loaded it.
    pub loaded_by: String,
    /// When.
    pub loaded_at: OffsetDateTime,
    /// How many artifacts it identifies, exactly or in part.
    pub matched_artifacts: i64,
}

/// What loading came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loaded {
    /// The DAT's new id.
    pub id: uuid::Uuid,
    /// Whether it replaced one of the same name.
    pub replaced: bool,
    /// How many matches its games made with the library's artifacts.
    pub matches: u64,
}

fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Load a DAT, replacing any of the same name, and match it against every
/// hashed artifact, in one transaction: a load cut short (a client that went
/// away, a failure) leaves the DAT it would have replaced, matches and all.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn load_dat(pool: &PgPool, dat: &NewDat<'_>) -> Result<Loaded, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let replaced = sqlx::query("DELETE FROM dat_files WHERE name = $1")
        .bind(dat.name)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?
        .rows_affected()
        > 0;
    let id = uuid::Uuid::now_v7();
    let rom_count: usize = dat.games.iter().map(|game| game.roms.len()).sum();
    sqlx::query(
        "INSERT INTO dat_files
             (id, name, description, version, author, homepage, url, source_sha256,
              game_count, rom_count, loaded_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(dat.name)
    .bind(dat.description)
    .bind(dat.version)
    .bind(dat.author)
    .bind(dat.homepage)
    .bind(dat.url)
    .bind(dat.source_sha256)
    .bind(count(dat.games.len()))
    .bind(count(rom_count))
    .bind(dat.loaded_by)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let game_ids: Vec<uuid::Uuid> = dat.games.iter().map(|_| uuid::Uuid::now_v7()).collect();
    for (ids, games) in game_ids.chunks(BATCH).zip(dat.games.chunks(BATCH)) {
        insert_games(&mut tx, id, ids, games).await?;
    }
    let mut pending: Vec<(uuid::Uuid, &NewRom<'_>)> = Vec::with_capacity(BATCH);
    for (game_id, game) in game_ids.iter().zip(&dat.games) {
        for rom in &game.roms {
            pending.push((*game_id, rom));
            if pending.len() == BATCH {
                insert_roms(&mut tx, &pending).await?;
                pending.clear();
            }
        }
    }
    if !pending.is_empty() {
        insert_roms(&mut tx, &pending).await?;
    }
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, 'dat.loaded', 'dat', $3, 'success',
                 jsonb_build_object('name', $4::text, 'games', $5::int, 'files', $6::int,
                                    'replaced', $7::bool, 'source_sha256', $8::text))",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(dat.loaded_by)
    .bind(id.to_string())
    .bind(dat.name)
    .bind(count(dat.games.len()))
    .bind(count(rom_count))
    .bind(replaced)
    .bind(dat.source_sha256)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let matches = rematch_in(&mut tx, None, Some(id)).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Loaded {
        id,
        replaced,
        matches,
    })
}

async fn insert_games(
    tx: &mut Transaction<'_, Postgres>,
    dat: uuid::Uuid,
    ids: &[uuid::Uuid],
    games: &[NewGame<'_>],
) -> Result<(), DbError> {
    let names: Vec<&str> = games.iter().map(|game| game.name).collect();
    let descriptions: Vec<Option<&str>> = games.iter().map(|game| game.description).collect();
    let categories: Vec<Option<&str>> = games.iter().map(|game| game.category).collect();
    sqlx::query(
        "INSERT INTO dat_games (id, dat_id, name, description, category)
         SELECT id, $1, name, description, category
         FROM UNNEST($2::uuid[], $3::text[], $4::text[], $5::text[])
             AS t(id, name, description, category)",
    )
    .bind(dat)
    .bind(ids)
    .bind(&names)
    .bind(&descriptions)
    .bind(&categories)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

async fn insert_roms(
    tx: &mut Transaction<'_, Postgres>,
    roms: &[(uuid::Uuid, &NewRom<'_>)],
) -> Result<(), DbError> {
    let games: Vec<uuid::Uuid> = roms.iter().map(|(game, _)| *game).collect();
    let names: Vec<&str> = roms.iter().map(|(_, rom)| rom.name).collect();
    let sizes: Vec<Option<i64>> = roms
        .iter()
        .map(|(_, rom)| rom.size.and_then(|size| i64::try_from(size).ok()))
        .collect();
    let crc32: Vec<Option<&str>> = roms.iter().map(|(_, rom)| rom.crc32).collect();
    let md5: Vec<Option<&str>> = roms.iter().map(|(_, rom)| rom.md5).collect();
    let sha1: Vec<Option<&str>> = roms.iter().map(|(_, rom)| rom.sha1).collect();
    let sha256: Vec<Option<&str>> = roms.iter().map(|(_, rom)| rom.sha256).collect();
    let status: Vec<Option<&str>> = roms.iter().map(|(_, rom)| rom.status).collect();
    let cue: Vec<bool> = roms.iter().map(|(_, rom)| rom.is_cue).collect();
    sqlx::query(
        "INSERT INTO dat_roms (game_id, name, size, crc32, md5, sha1, sha256, status, is_cue)
         SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::bigint[], $4::text[], $5::text[],
                              $6::text[], $7::text[], $8::text[], $9::bool[])",
    )
    .bind(&games)
    .bind(&names)
    .bind(&sizes)
    .bind(&crc32)
    .bind(&md5)
    .bind(&sha1)
    .bind(&sha256)
    .bind(&status)
    .bind(&cue)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

fn dat_from_row(row: &sqlx::postgres::PgRow) -> Result<DatRecord, DbError> {
    let get = |error| DbError::Query(error);
    Ok(DatRecord {
        id: row.try_get("id").map_err(get)?,
        name: row.try_get("name").map_err(get)?,
        description: row.try_get("description").map_err(get)?,
        version: row.try_get("version").map_err(get)?,
        author: row.try_get("author").map_err(get)?,
        homepage: row.try_get("homepage").map_err(get)?,
        source_sha256: row.try_get("source_sha256").map_err(get)?,
        game_count: row.try_get("game_count").map_err(get)?,
        rom_count: row.try_get("rom_count").map_err(get)?,
        loaded_by: row.try_get("loaded_by").map_err(get)?,
        loaded_at: row.try_get("loaded_at").map_err(get)?,
        matched_artifacts: row.try_get("matched_artifacts").map_err(get)?,
    })
}

const DAT_COLUMNS: &str = "d.id, d.name, d.description, d.version, d.author, d.homepage, \
     d.source_sha256, d.game_count, d.rom_count, d.loaded_by, d.loaded_at, \
     (SELECT count(DISTINCT m.artifact_id) FROM artifact_dat_matches m \
      JOIN dat_games g ON g.id = m.game_id WHERE g.dat_id = d.id) AS matched_artifacts";

/// Every DAT, by name.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_dats(pool: &PgPool) -> Result<Vec<DatRecord>, DbError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {DAT_COLUMNS} FROM dat_files d ORDER BY d.name"
    )))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(dat_from_row).collect()
}

/// One DAT.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_dat(pool: &PgPool, id: uuid::Uuid) -> Result<Option<DatRecord>, DbError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {DAT_COLUMNS} FROM dat_files d WHERE d.id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(dat_from_row).transpose()
}

/// Remove a DAT, its games, files and matches, as `actor`. Answers whether
/// there was one.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn delete_dat(pool: &PgPool, id: uuid::Uuid, actor: &str) -> Result<bool, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let name: Option<String> =
        sqlx::query_scalar("DELETE FROM dat_files WHERE id = $1 RETURNING name")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    let Some(name) = name else {
        return Ok(false);
    };
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, 'dat.removed', 'dat', $3, 'success',
                 jsonb_build_object('name', $4::text))",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor)
    .bind(id.to_string())
    .bind(name)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(true)
}

/// The units of the artifacts in `$1` (all hashed artifacts when null): CD
/// tracks where an artifact has them, otherwise its image files: not a
/// descriptor, metadata, a subchannel or a certificate tree, which no DAT
/// lists a dump by.
const UNITS: &str = "units AS (
    SELECT t.artifact_id, t.sha1, t.length_bytes,
           'track ' || t.track_number AS unit
    FROM artifact_track_hashes t
    WHERE $1::uuid[] IS NULL OR t.artifact_id = ANY($1)
    UNION ALL
    SELECT h.artifact_id, h.value, c.length_bytes, c.logical_path
    FROM artifact_hashes h
    JOIN artifact_components c ON c.id = h.component_id
    WHERE h.algorithm = 'sha1'
      AND c.role NOT IN ('descriptor', 'metadata', 'subchannel', 'certificate_tree')
      AND ($1::uuid[] IS NULL OR h.artifact_id = ANY($1))
      AND NOT EXISTS (SELECT 1 FROM artifact_track_hashes x WHERE x.artifact_id = h.artifact_id)
)";

/// Recompute the matches of `artifacts` (every hashed artifact when
/// `None`) against the games of `dat` (every DAT when `None`), replacing
/// those already recorded in that scope, in a transaction of its own.
/// Answers how many matches there are now in it.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn rematch(
    pool: &PgPool,
    artifacts: Option<&[uuid::Uuid]>,
    dat: Option<uuid::Uuid>,
) -> Result<u64, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let matches = rematch_in(&mut tx, artifacts, dat).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(matches)
}

/// [`rematch`], inside the caller's transaction, so what changed the
/// matches (a DAT loaded, an artifact hashed) and the matches commit
/// together or not at all.
///
/// How many of a game's files an artifact matches: within one SHA-1, any
/// unit can stand for any rom, so each SHA-1 matches as many files as the
/// fewer of its roms and its units (two identical silent tracks in the DAT
/// and one in the artifact match one file, not two). A game is `exact`
/// when that is every file it has, its CUE aside, and the artifact has no
/// units left over.
///
/// A match another transaction recorded meanwhile is updated rather than
/// duplicated, so an artifact hashed while a DAT loads loses neither.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn rematch_in(
    tx: &mut Transaction<'_, Postgres>,
    artifacts: Option<&[uuid::Uuid]>,
    dat: Option<uuid::Uuid>,
) -> Result<u64, DbError> {
    sqlx::query(
        "DELETE FROM artifact_dat_matches m
         USING dat_games g
         WHERE g.id = m.game_id
           AND ($1::uuid[] IS NULL OR m.artifact_id = ANY($1))
           AND ($2::uuid IS NULL OR g.dat_id = $2)",
    )
    .bind(artifacts)
    .bind(dat)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    let inserted = sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH {UNITS},
         unit_counts AS (SELECT artifact_id, count(*) AS n FROM units GROUP BY artifact_id),
         classes AS (
             SELECT u.artifact_id, r.game_id, r.sha1,
                    count(DISTINCT r.id) AS roms, count(DISTINCT u.unit) AS units
             FROM units u
             JOIN dat_roms r ON r.sha1 = u.sha1 AND NOT r.is_cue
                            AND (r.size IS NULL OR r.size = u.length_bytes)
             JOIN dat_games g ON g.id = r.game_id
             WHERE $2::uuid IS NULL OR g.dat_id = $2
             GROUP BY u.artifact_id, r.game_id, r.sha1
         ),
         hits AS (
             SELECT artifact_id, game_id, sum(LEAST(roms, units)) AS matched
             FROM classes
             GROUP BY artifact_id, game_id
         ),
         totals AS (
             SELECT r.game_id, count(*) AS total
             FROM dat_roms r
             WHERE NOT r.is_cue AND r.game_id IN (SELECT game_id FROM hits)
             GROUP BY r.game_id
         )
         INSERT INTO artifact_dat_matches (artifact_id, game_id, kind, roms_matched, roms_total)
         SELECT h.artifact_id, h.game_id,
                CASE WHEN h.matched = t.total AND u.n = t.total THEN 'exact' ELSE 'partial' END,
                LEAST(h.matched, t.total), t.total
         FROM hits h
         JOIN totals t ON t.game_id = h.game_id
         JOIN unit_counts u ON u.artifact_id = h.artifact_id
         ON CONFLICT (artifact_id, game_id) DO UPDATE
             SET kind = EXCLUDED.kind, roms_matched = EXCLUDED.roms_matched,
                 roms_total = EXCLUDED.roms_total"
    )))
    .bind(artifacts)
    .bind(dat)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    Ok(inserted.rows_affected())
}

/// One file of a matched game, and what of the artifact it matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRom {
    /// Its name in the DAT.
    pub name: String,
    /// Its size.
    pub size: Option<i64>,
    /// The track (`track 2`) or file of the artifact with its SHA-1 and
    /// size, if any.
    pub matched_by: Option<String>,
}

/// A game an artifact matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchRecord {
    /// The DAT.
    pub dat_id: uuid::Uuid,
    /// Its name.
    pub dat_name: String,
    /// Its version.
    pub dat_version: Option<String>,
    /// The game.
    pub game_id: uuid::Uuid,
    /// Its name.
    pub game_name: String,
    /// Its description.
    pub description: Option<String>,
    /// Its category.
    pub category: Option<String>,
    /// `exact` or `partial`.
    pub kind: String,
    /// Files matched.
    pub roms_matched: i32,
    /// Files the game has, its CUE aside.
    pub roms_total: i32,
    /// Each file, its CUE aside.
    pub roms: Vec<MatchedRom>,
}

/// The games `artifact` matches, exact first, then by files matched.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn matches_for(pool: &PgPool, artifact: uuid::Uuid) -> Result<Vec<MatchRecord>, DbError> {
    let get = |error| DbError::Query(error);
    let rows = sqlx::query(
        "SELECT d.id AS dat_id, d.name AS dat_name, d.version AS dat_version,
                g.id AS game_id, g.name AS game_name, g.description, g.category,
                m.kind, m.roms_matched, m.roms_total
         FROM artifact_dat_matches m
         JOIN dat_games g ON g.id = m.game_id
         JOIN dat_files d ON d.id = g.dat_id
         WHERE m.artifact_id = $1
         ORDER BY (m.kind = 'exact') DESC, m.roms_matched DESC, d.name, g.name
         LIMIT 50",
    )
    .bind(artifact)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    let mut matches = Vec::with_capacity(rows.len());
    for row in &rows {
        matches.push(MatchRecord {
            dat_id: row.try_get("dat_id").map_err(get)?,
            dat_name: row.try_get("dat_name").map_err(get)?,
            dat_version: row.try_get("dat_version").map_err(get)?,
            game_id: row.try_get("game_id").map_err(get)?,
            game_name: row.try_get("game_name").map_err(get)?,
            description: row.try_get("description").map_err(get)?,
            category: row.try_get("category").map_err(get)?,
            kind: row.try_get("kind").map_err(get)?,
            roms_matched: row.try_get("roms_matched").map_err(get)?,
            roms_total: row.try_get("roms_total").map_err(get)?,
            roms: Vec::new(),
        });
    }
    if matches.is_empty() {
        return Ok(matches);
    }
    let games: Vec<uuid::Uuid> = matches.iter().map(|m| m.game_id).collect();
    // Each file paired with at most one unit, and each unit with at most
    // one file: within a SHA-1, the k-th file (by its order in the DAT) with
    // the k-th unit, as the counting in rematch_in assumes.
    let files = sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH {UNITS},
         ranked_units AS (
             SELECT sha1, length_bytes, unit,
                    row_number() OVER (PARTITION BY sha1 ORDER BY unit) AS k
             FROM units
         ),
         ranked_roms AS (
             SELECT r.game_id, r.id, r.name, r.size, r.sha1,
                    row_number() OVER (PARTITION BY r.game_id, r.sha1 ORDER BY r.id) AS k
             FROM dat_roms r
             WHERE r.game_id = ANY($2) AND NOT r.is_cue
         )
         SELECT rr.game_id, rr.name, rr.size, ru.unit AS matched_by
         FROM ranked_roms rr
         LEFT JOIN ranked_units ru
           ON ru.sha1 = rr.sha1 AND ru.k = rr.k
          AND (rr.size IS NULL OR rr.size = ru.length_bytes)
         ORDER BY rr.game_id, rr.id"
    )))
    .bind(vec![artifact])
    .bind(&games)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    let mut by_game: BTreeMap<uuid::Uuid, Vec<MatchedRom>> = BTreeMap::new();
    for row in &files {
        by_game
            .entry(row.try_get("game_id").map_err(get)?)
            .or_default()
            .push(MatchedRom {
                name: row.try_get("name").map_err(get)?,
                size: row.try_get("size").map_err(get)?,
                matched_by: row.try_get("matched_by").map_err(get)?,
            });
    }
    for record in &mut matches {
        record.roms = by_game.remove(&record.game_id).unwrap_or_default();
    }
    Ok(matches)
}

/// The best match of each of `artifacts` that has one: an exact one first,
/// then the most files matched. By artifact: the game's name, `exact` or
/// `partial`, and the DAT's name.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn best_matches(
    pool: &PgPool,
    artifacts: &[uuid::Uuid],
) -> Result<BTreeMap<uuid::Uuid, (String, String, String)>, DbError> {
    if artifacts.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows = sqlx::query(
        "SELECT DISTINCT ON (m.artifact_id) m.artifact_id, g.name AS game, m.kind, d.name AS dat
         FROM artifact_dat_matches m
         JOIN dat_games g ON g.id = m.game_id
         JOIN dat_files d ON d.id = g.dat_id
         WHERE m.artifact_id = ANY($1)
         ORDER BY m.artifact_id, (m.kind = 'exact') DESC, m.roms_matched DESC, g.name",
    )
    .bind(artifacts)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    let get = |error| DbError::Query(error);
    let mut best = BTreeMap::new();
    for row in &rows {
        best.insert(
            row.try_get("artifact_id").map_err(get)?,
            (
                row.try_get("game").map_err(get)?,
                row.try_get("kind").map_err(get)?,
                row.try_get("dat").map_err(get)?,
            ),
        );
    }
    Ok(best)
}
