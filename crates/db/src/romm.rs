// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The RomM export's state: which editions should appear in RomM, what was
//! written for each, and a receipt for every file put in place.

use serde_json::Value;
use sqlx::{PgPool, Row};
use std::str::FromStr;
use tangible_domain::{ArtifactFormat, ArtifactId, DiscId, EditionId, LossCharacter};
use time::OffsetDateTime;

use crate::DbError;

/// The integration row the filesystem export records its receipts under.
pub const INTEGRATION_NAME: &str = "romm-filesystem";

/// An edition's RomM settings and how its export stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionRomm {
    /// The edition.
    pub edition_id: EditionId,
    /// Its platform, as a RomM slug.
    pub platform: Option<String>,
    /// Whether it should appear in RomM.
    pub export: bool,
    /// What the export last did, if it has run.
    pub status: Option<ExportStatus>,
}

/// What the export last did for one edition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportStatus {
    /// `current`, `blocked`, `failed` or `removed`.
    pub state: String,
    /// The folder, relative to the export root.
    pub folder: Option<String>,
    /// Why it is blocked or failed.
    pub detail: Option<String>,
    /// Files written.
    pub file_count: i32,
    /// When it was last written.
    pub exported_at: Option<OffsetDateTime>,
    /// When it was last checked.
    pub checked_at: OffsetDateTime,
    /// What RomM made of it, once RomM's API has been asked.
    pub link: Option<RommLink>,
}

/// What RomM made of an export, as last looked up through its API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RommLink {
    /// `unseen`, `linked`, `missing` or `differs`.
    pub state: String,
    /// RomM's id for the ROM, when one was found.
    pub rom_id: Option<i64>,
    /// How RomM's files differ, or what else there is to say.
    pub detail: Option<String>,
    /// When RomM was asked.
    pub checked_at: OffsetDateTime,
}

/// An export to look up in RomM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget {
    /// The edition.
    pub edition_id: EditionId,
    /// The export's state: `current` or `removed`.
    pub state: String,
    /// Its folder, `platform/game`, while it is current.
    pub folder: Option<String>,
    /// RomM's id for its ROM, as last found.
    pub rom_id: Option<i64>,
    /// What RomM made of it last time.
    pub romm_state: Option<String>,
}

/// A file an export holds, as its receipt has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedFile {
    /// Its name in the game's folder.
    pub name: String,
    /// Its size, when the receipt recorded one.
    pub length_bytes: Option<i64>,
}

/// What to record of looking an export up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewLink<'a> {
    /// `unseen`, `linked`, `missing` or `differs`.
    pub state: &'a str,
    /// RomM's id for the ROM; none for `unseen`.
    pub rom_id: Option<i64>,
    /// What there is to say.
    pub detail: Option<&'a str>,
}

/// Everything the export needs to know about an edition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportInputs {
    /// The title's display name.
    pub title: String,
    /// The edition's region.
    pub region: Option<String>,
    /// Its platform.
    pub platform: Option<String>,
    /// Whether it should appear in RomM.
    pub export: bool,
    /// Its discs, in disc order across its sets.
    pub discs: Vec<DiscInputs>,
}

/// One disc and the artifacts linked to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscInputs {
    /// The disc.
    pub disc_id: DiscId,
    /// Linked artifacts, in the order they were linked.
    pub artifacts: Vec<LinkedArtifact>,
}

/// An artifact linked to a disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedArtifact {
    /// The artifact.
    pub id: ArtifactId,
    /// Its format.
    pub format: ArtifactFormat,
    /// The link's relationship, such as `representation_of`.
    pub relationship: String,
    /// When it is a derivative, what it was shown to preserve of its parent.
    pub loss_character: Option<LossCharacter>,
}

/// One file an export put in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// Its path relative to the export root.
    pub destination: String,
    /// The artifact it came from.
    pub artifact_id: ArtifactId,
    /// `iso` or `cue_bin`.
    pub representation: String,
    /// `hardlink` or `copy`.
    pub method: String,
    /// The digest of the bytes written.
    pub sha256: String,
    /// The number of bytes written.
    pub length_bytes: i64,
}

/// Set an edition's platform and whether it should appear in RomM.
///
/// Returns `None` for an edition that does not exist.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, including the constraint that
/// refuses an export without a platform.
pub async fn set_edition_romm(
    pool: &PgPool,
    edition_id: EditionId,
    platform: Option<&str>,
    export: bool,
    actor: &str,
) -> Result<Option<EditionRomm>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let updated = sqlx::query("UPDATE editions SET platform = $2, romm_export = $3 WHERE id = $1")
        .bind(edition_id.as_uuid())
        .bind(platform)
        .bind(export)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    if updated.rows_affected() == 0 {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(None);
    }
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, 'edition.romm_export_set', 'edition', $3, 'success', $4)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor)
    .bind(edition_id.to_string())
    .bind(serde_json::json!({ "platform": platform, "export": export }))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    edition_romm(pool, edition_id).await
}

/// An edition's RomM settings and export status.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn edition_romm(
    pool: &PgPool,
    edition_id: EditionId,
) -> Result<Option<EditionRomm>, DbError> {
    let row = sqlx::query(
        "SELECT e.platform, e.romm_export, x.state, x.folder, x.detail, x.file_count,
                x.exported_at, x.checked_at, x.romm_rom_id, x.romm_state, x.romm_detail,
                x.romm_checked_at
         FROM editions e LEFT JOIN romm_edition_exports x ON x.edition_id = e.id
         WHERE e.id = $1",
    )
    .bind(edition_id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.map(|row| {
        let state: Option<String> = row.try_get("state").map_err(DbError::Query)?;
        let romm_state: Option<String> = row.try_get("romm_state").map_err(DbError::Query)?;
        let link = match (
            romm_state,
            row.try_get("romm_checked_at").map_err(DbError::Query)?,
        ) {
            (Some(state), Some(checked_at)) => Some(RommLink {
                state,
                rom_id: row.try_get("romm_rom_id").map_err(DbError::Query)?,
                detail: row.try_get("romm_detail").map_err(DbError::Query)?,
                checked_at,
            }),
            _ => None,
        };
        Ok(EditionRomm {
            edition_id,
            platform: row.try_get("platform").map_err(DbError::Query)?,
            export: row.try_get("romm_export").map_err(DbError::Query)?,
            status: match state {
                None => None,
                Some(state) => Some(ExportStatus {
                    state,
                    folder: row.try_get("folder").map_err(DbError::Query)?,
                    detail: row.try_get("detail").map_err(DbError::Query)?,
                    file_count: row.try_get("file_count").map_err(DbError::Query)?,
                    exported_at: row.try_get("exported_at").map_err(DbError::Query)?,
                    checked_at: row.try_get("checked_at").map_err(DbError::Query)?,
                    link,
                }),
            },
        })
    })
    .transpose()
}

/// Editions the export has to look at: every one that should be in RomM,
/// and every one that was and is not taken out yet.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn editions_to_reconcile(pool: &PgPool) -> Result<Vec<EditionId>, DbError> {
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT e.id FROM editions e
         LEFT JOIN romm_edition_exports x ON x.edition_id = e.id
         WHERE e.romm_export
            OR (x.state IS NOT NULL AND x.state <> 'removed' AND x.folder IS NOT NULL)
         ORDER BY e.id",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(ids.into_iter().map(EditionId::from_uuid).collect())
}

/// Everything the export needs about one edition.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or [`DbError::Enum`] for a
/// stored format this build does not know.
pub async fn export_inputs(
    pool: &PgPool,
    edition_id: EditionId,
) -> Result<Option<ExportInputs>, DbError> {
    let Some(row) = sqlx::query(
        "SELECT t.display_title, e.region, e.platform, e.romm_export
         FROM editions e JOIN titles t ON t.id = e.title_id WHERE e.id = $1",
    )
    .bind(edition_id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?
    else {
        return Ok(None);
    };

    let discs: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT d.id FROM discs d JOIN disc_sets s ON s.id = d.disc_set_id
         WHERE s.edition_id = $1
         ORDER BY s.created_at, s.id, d.sequence_number, d.id",
    )
    .bind(edition_id.as_uuid())
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    let links = sqlx::query(
        "SELECT l.disc_id, a.id, a.format, l.relationship,
                (SELECT d.loss_character FROM derivations d
                 WHERE d.child_artifact_id = a.id AND NOT d.superseded
                 ORDER BY d.started_at DESC LIMIT 1) AS loss_character
         FROM artifact_disc_links l JOIN artifacts a ON a.id = l.artifact_id
         WHERE l.disc_id = ANY($1)
           -- Neither an artifact being deleted nor a deleted one is a disc's
           -- image any longer (ADR 0020).
           AND NOT a.pending_deletion AND a.deleted_at IS NULL
         ORDER BY l.created_at, a.id",
    )
    .bind(&discs)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    let mut out: Vec<DiscInputs> = discs
        .iter()
        .map(|id| DiscInputs {
            disc_id: DiscId::from_uuid(*id),
            artifacts: Vec::new(),
        })
        .collect();
    for link in &links {
        let disc: uuid::Uuid = link.try_get("disc_id").map_err(DbError::Query)?;
        let artifact: uuid::Uuid = link.try_get("id").map_err(DbError::Query)?;
        let format: String = link.try_get("format").map_err(DbError::Query)?;
        let format = ArtifactFormat::from_str(&format).map_err(|_| DbError::Enum {
            column: "artifacts.format",
            value: format.clone(),
        })?;
        let relationship: String = link.try_get("relationship").map_err(DbError::Query)?;
        let loss: Option<String> = link.try_get("loss_character").map_err(DbError::Query)?;
        let loss_character = loss
            .map(|value| {
                LossCharacter::from_str(&value).map_err(|_| DbError::Enum {
                    column: "derivations.loss_character",
                    value,
                })
            })
            .transpose()?;
        if let Some(entry) = out
            .iter_mut()
            .find(|entry| *entry.disc_id.as_uuid() == disc)
        {
            entry.artifacts.push(LinkedArtifact {
                id: ArtifactId::from_uuid(artifact),
                format,
                relationship,
                loss_character,
            });
        }
    }

    Ok(Some(ExportInputs {
        title: row.try_get("display_title").map_err(DbError::Query)?,
        region: row.try_get("region").map_err(DbError::Query)?,
        platform: row.try_get("platform").map_err(DbError::Query)?,
        export: row.try_get("romm_export").map_err(DbError::Query)?,
        discs: out,
    }))
}

/// Record how an edition's export stands.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_status(
    pool: &PgPool,
    edition_id: EditionId,
    state: &str,
    folder: Option<&str>,
    detail: Option<&str>,
    file_count: i32,
    written: bool,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO romm_edition_exports
             (edition_id, state, folder, detail, file_count, exported_at, checked_at)
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $6 THEN now() END, now())
         ON CONFLICT (edition_id) DO UPDATE SET
             state = EXCLUDED.state,
             folder = EXCLUDED.folder,
             detail = EXCLUDED.detail,
             file_count = EXCLUDED.file_count,
             exported_at = COALESCE(EXCLUDED.exported_at, romm_edition_exports.exported_at),
             checked_at = now()",
    )
    .bind(edition_id.as_uuid())
    .bind(state)
    .bind(folder)
    .bind(detail.map(|text| text.chars().take(2000).collect::<String>()))
    .bind(file_count)
    .bind(written)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// The integration row receipts are filed under, made if it is not there.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn ensure_integration(
    pool: &PgPool,
    configuration: &Value,
) -> Result<uuid::Uuid, DbError> {
    sqlx::query_scalar(
        "INSERT INTO integrations (id, kind, name, enabled, configuration_json, status)
         VALUES ($1, 'romm', $2, TRUE, $3, 'ok')
         ON CONFLICT (name) DO UPDATE SET
             enabled = TRUE, configuration_json = EXCLUDED.configuration_json
         RETURNING id",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(INTEGRATION_NAME)
    .bind(configuration)
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)
}

/// Record the files an export now holds in `folder`, and that any it held
/// there before and does not now are gone.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_receipts(
    pool: &PgPool,
    integration_id: uuid::Uuid,
    folder: &str,
    receipts: &[Receipt],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    for receipt in receipts {
        sqlx::query(
            "INSERT INTO export_receipts
                 (id, integration_id, artifact_id, destination_key, representation,
                  materialization_method, content_fingerprint, state, length_bytes)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'present', $8)
             ON CONFLICT (integration_id, destination_key) DO UPDATE SET
                 artifact_id = EXCLUDED.artifact_id,
                 representation = EXCLUDED.representation,
                 materialization_method = EXCLUDED.materialization_method,
                 content_fingerprint = EXCLUDED.content_fingerprint,
                 length_bytes = EXCLUDED.length_bytes,
                 state = 'present'",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(integration_id)
        .bind(receipt.artifact_id.as_uuid())
        .bind(&receipt.destination)
        .bind(&receipt.representation)
        .bind(&receipt.method)
        .bind(&receipt.sha256)
        .bind(receipt.length_bytes)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }
    let present: Vec<&str> = receipts.iter().map(|r| r.destination.as_str()).collect();
    mark_removed(&mut tx, integration_id, folder, &present).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(())
}

/// Give the receipts of an unchanged export the sizes they were written
/// before receipts recorded any.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn fill_receipt_lengths(
    pool: &PgPool,
    integration_id: uuid::Uuid,
    receipts: &[Receipt],
) -> Result<(), DbError> {
    let destinations: Vec<&str> = receipts.iter().map(|r| r.destination.as_str()).collect();
    let lengths: Vec<i64> = receipts.iter().map(|r| r.length_bytes).collect();
    sqlx::query(
        "UPDATE export_receipts r SET length_bytes = f.length_bytes
         FROM unnest($2::text[], $3::bigint[]) AS f(destination_key, length_bytes)
         WHERE r.integration_id = $1 AND r.destination_key = f.destination_key
           AND r.length_bytes IS NULL AND r.state = 'present'",
    )
    .bind(integration_id)
    .bind(&destinations)
    .bind(&lengths)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// The exports to look up in RomM: every current one when `all`, else
/// those not linked or written since they were last looked up; and every
/// removed one whose ROM RomM has not yet marked missing, or that still
/// has something recorded to clear.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn exports_to_link(pool: &PgPool, all: bool) -> Result<Vec<LinkTarget>, DbError> {
    let rows = sqlx::query(
        "SELECT edition_id, state, folder, romm_rom_id, romm_state FROM romm_edition_exports
         WHERE (state = 'current' AND folder IS NOT NULL
                AND ($1 OR romm_checked_at IS NULL OR romm_state <> 'linked'
                     OR exported_at > romm_checked_at))
            OR (state = 'removed'
                AND ((romm_rom_id IS NOT NULL AND romm_state <> 'missing')
                     OR (romm_rom_id IS NULL AND romm_state IS NOT NULL)))
         ORDER BY edition_id",
    )
    .bind(all)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            let get = DbError::Query;
            Ok(LinkTarget {
                edition_id: EditionId::from_uuid(row.try_get("edition_id").map_err(get)?),
                state: row.try_get("state").map_err(get)?,
                folder: row.try_get("folder").map_err(get)?,
                rom_id: row.try_get("romm_rom_id").map_err(get)?,
                romm_state: row.try_get("romm_state").map_err(get)?,
            })
        })
        .collect()
}

/// The files the export holds in `folder`, by name, from their receipts.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn exported_files(pool: &PgPool, folder: &str) -> Result<Vec<ExportedFile>, DbError> {
    let prefix = format!("{folder}/");
    let rows = sqlx::query(
        "SELECT r.destination_key, r.length_bytes
         FROM export_receipts r JOIN integrations i ON i.id = r.integration_id
         WHERE i.name = $1 AND r.state = 'present' AND left(r.destination_key, $3) = $2
         ORDER BY r.destination_key",
    )
    .bind(INTEGRATION_NAME)
    .bind(&prefix)
    .bind(i32::try_from(prefix.chars().count()).unwrap_or(i32::MAX))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .filter_map(|row| {
            let destination: String = match row.try_get("destination_key") {
                Ok(destination) => destination,
                Err(error) => return Some(Err(DbError::Query(error))),
            };
            let name = destination.strip_prefix(&prefix)?;
            // A file in a folder below the game's is not one the export
            // writes.
            if name.contains('/') {
                return None;
            }
            Some(
                row.try_get("length_bytes")
                    .map_err(DbError::Query)
                    .map(|length_bytes| ExportedFile {
                        name: name.to_owned(),
                        length_bytes,
                    }),
            )
        })
        .collect()
}

/// Record what looking `target` up in RomM found, or clear it with `None`.
/// Nothing is recorded if the export changed since it was read: the next
/// pass looks again. Returns whether it was recorded.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, including a link that breaks
/// the table's constraints.
pub async fn record_link(
    pool: &PgPool,
    target: &LinkTarget,
    link: Option<NewLink<'_>>,
) -> Result<bool, DbError> {
    let done = sqlx::query(
        "UPDATE romm_edition_exports SET
             romm_state = $4, romm_rom_id = $5, romm_detail = $6,
             romm_checked_at = CASE WHEN $4::text IS NULL THEN NULL ELSE now() END
         WHERE edition_id = $1 AND state = $2 AND folder IS NOT DISTINCT FROM $3",
    )
    .bind(target.edition_id.as_uuid())
    .bind(&target.state)
    .bind(target.folder.as_deref())
    .bind(link.map(|link| link.state))
    .bind(link.and_then(|link| link.rom_id))
    .bind(
        link.and_then(|link| link.detail)
            .map(|text| text.chars().take(2000).collect::<String>()),
    )
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(done.rows_affected() == 1)
}

/// Forget every link to RomM's ROMs: RomM's API was removed, or now points
/// at another RomM, whose ids are its own.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn forget_romm_links(pool: &PgPool) -> Result<u64, DbError> {
    let done = sqlx::query(
        "UPDATE romm_edition_exports SET
             romm_state = NULL, romm_rom_id = NULL, romm_detail = NULL, romm_checked_at = NULL
         WHERE romm_state IS NOT NULL OR romm_rom_id IS NOT NULL",
    )
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(done.rows_affected())
}

/// Record that everything an export held in `folder` is gone.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_folder_removed(
    pool: &PgPool,
    integration_id: uuid::Uuid,
    folder: &str,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    mark_removed(&mut tx, integration_id, folder, &[]).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(())
}

async fn mark_removed(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    integration_id: uuid::Uuid,
    folder: &str,
    keep: &[&str],
) -> Result<(), DbError> {
    // A prefix match on the folder and a slash, with LIKE's wildcards in the
    // folder name escaped, so "Game" does not match "Game II".
    let pattern = format!(
        "{}/%",
        folder
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    sqlx::query(
        "UPDATE export_receipts SET state = 'removed'
         WHERE integration_id = $1 AND destination_key LIKE $2 ESCAPE '\\'
           AND state <> 'removed' AND NOT (destination_key = ANY($3))",
    )
    .bind(integration_id)
    .bind(pattern)
    .bind(keep)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// What `chdman verify` has said of one CHD, by every chdman that ran it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChdVerdicts {
    /// Whether any run passed.
    pub passed: bool,
    /// The versions that have run it.
    pub tried_with: Vec<String>,
    /// What the most recent failure said, if one failed.
    pub last_failure: Option<String>,
}

/// What `chdman verify` has said of the CHD with this SHA-256.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn chd_verdicts(pool: &PgPool, sha256: &str) -> Result<ChdVerdicts, DbError> {
    let rows: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT tool_version, passed, detail FROM chd_verifications
         WHERE sha256 = $1 ORDER BY created_at, tool_version",
    )
    .bind(sha256)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    let mut verdicts = ChdVerdicts::default();
    for (version, passed, detail) in rows {
        verdicts.passed |= passed;
        if !passed {
            verdicts.last_failure = detail.or_else(|| Some("it did not verify".to_owned()));
        }
        verdicts.tried_with.push(version);
    }
    Ok(verdicts)
}

/// Record what `chdman verify` at `tool_version` said of a CHD. A second
/// verdict from the same version changes nothing: it is about the same bytes.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_chd_verdict(
    pool: &PgPool,
    sha256: &str,
    tool_version: &str,
    passed: bool,
    detail: Option<&str>,
) -> Result<(), DbError> {
    let detail: Option<String> = detail.map(|detail| detail.chars().take(2000).collect());
    sqlx::query(
        "INSERT INTO chd_verifications (sha256, tool_version, passed, detail)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (sha256, tool_version) DO NOTHING",
    )
    .bind(sha256)
    .bind(tool_version)
    .bind(passed)
    .bind(detail)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}
