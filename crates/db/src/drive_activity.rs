// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What each drive is doing now (docs/plans/ui-overhaul.md, S1): the open
//! burn attempt, rip or erasure on it, for the web UI's drive bay.
//!
//! A drive has at most one of each open at a time, and the database says so
//! (`burn_attempts_one_active_per_drive`, `rip_jobs_one_open_per_drive`,
//! `media_erasures_one_open_per_drive`); the scheduler keeps a rip and a burn
//! off one drive (AGENTS.md 4.5). Should two ever be open, the one started
//! last is reported.

use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use tangible_domain::DriveId;

use crate::DbError;

/// The kind of work a drive is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    /// Writing a disc, from claiming the job to verifying what was written.
    Burn,
    /// Reading a disc, from the request to the upload.
    Rip,
    /// Erasing a rewritable disc.
    Erasure,
}

impl ActivityKind {
    /// As the API spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Burn => "burn",
            Self::Rip => "rip",
            Self::Erasure => "erasure",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "burn" => Some(Self::Burn),
            "rip" => Some(Self::Rip),
            "erasure" => Some(Self::Erasure),
            _ => None,
        }
    }
}

/// One drive's open work.
#[derive(Debug, Clone, PartialEq)]
pub struct DriveActivityRecord {
    /// The drive.
    pub drive_id: DriveId,
    /// What it is doing.
    pub kind: ActivityKind,
    /// The burn job, the rip or the erasure.
    pub id: Uuid,
    /// Its state. For a burn, the job's, which is the one that says it is
    /// waiting for a disc.
    pub state: String,
    /// For a burn, the title of the disc being burned.
    pub title: Option<String>,
    /// For a burn, which disc of the set.
    pub disc_number: Option<i16>,
    /// How far the worker last said it had got, from 0 to 1.
    pub progress: Option<f32>,
    /// The disc in the drive as the worker inspected it, as recorded: the
    /// burn engine's or rip engine's medium report. Never key material.
    pub medium: Option<serde_json::Value>,
    /// For a burn, the artifact being written, whose manifest says what the
    /// disc will hold.
    pub artifact_id: Option<Uuid>,
    /// When the work began, or was asked for if it has not begun.
    pub started_at: OffsetDateTime,
}

/// What every drive with open work is doing.
///
/// `medium_inspected` is the burn event code whose data carries the
/// inspected medium (`tangible_burn::runner::MEDIUM_INSPECTED`), passed in so
/// this crate does not depend on the burn engine.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn drive_activities(
    pool: &PgPool,
    medium_inspected: &str,
) -> Result<Vec<DriveActivityRecord>, DbError> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (drive_id) drive_id, kind, id, state, title, disc_number,
                progress, medium, artifact_id, started_at
         FROM (
             SELECT a.drive_id, 'burn' AS kind, j.id, j.state,
                    t.display_title AS title, d.sequence_number AS disc_number,
                    (SELECT e.progress_fraction FROM burn_events e
                     WHERE e.attempt_id = a.id AND e.progress_fraction IS NOT NULL
                     ORDER BY e.sequence DESC LIMIT 1) AS progress,
                    (SELECT e.data_json -> 'medium' FROM burn_events e
                     WHERE e.attempt_id = a.id AND e.message_code = $1
                     ORDER BY e.sequence DESC LIMIT 1) AS medium,
                    j.artifact_id,
                    a.started_at
             FROM burn_attempts a
             JOIN burn_jobs j ON j.id = a.burn_job_id
             JOIN discs d ON d.id = j.disc_id
             JOIN disc_sets s ON s.id = d.disc_set_id
             JOIN editions ed ON ed.id = s.edition_id
             JOIN titles t ON t.id = ed.title_id
             WHERE a.state IN ('claimed', 'staging', 'preflighting', 'writing', 'written',
                               'verifying')
           UNION ALL
             SELECT r.drive_id, 'rip', r.id, r.state, NULL, NULL,
                    (SELECT e.progress_fraction FROM rip_events e
                     WHERE e.rip_id = r.id AND e.progress_fraction IS NOT NULL
                     ORDER BY e.sequence DESC LIMIT 1),
                    r.medium_json, NULL::uuid,
                    COALESCE(r.started_at, r.created_at)
             FROM rip_jobs r
             WHERE r.state IN ('queued', 'reading', 'uploading')
           UNION ALL
             SELECT m.drive_id, 'erasure', m.id, m.state, NULL, NULL, NULL, NULL,
                    NULL::uuid, COALESCE(m.started_at, m.created_at)
             FROM media_erasures m
             WHERE m.state IN ('queued', 'erasing')
         ) AS open_work
         ORDER BY drive_id, started_at DESC",
    )
    .bind(medium_inspected)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    rows.iter()
        .map(|row| {
            let get = DbError::Query;
            let kind: String = row.try_get("kind").map_err(get)?;
            Ok(DriveActivityRecord {
                drive_id: DriveId::from_uuid(row.try_get("drive_id").map_err(get)?),
                kind: ActivityKind::parse(&kind).ok_or(DbError::Enum {
                    column: "drive activity kind",
                    value: kind,
                })?,
                id: row.try_get("id").map_err(get)?,
                state: row.try_get("state").map_err(get)?,
                title: row.try_get("title").map_err(get)?,
                disc_number: row.try_get("disc_number").map_err(get)?,
                progress: row.try_get("progress").map_err(get)?,
                medium: row.try_get("medium").map_err(get)?,
                artifact_id: row.try_get("artifact_id").map_err(get)?,
                started_at: row.try_get("started_at").map_err(get)?,
            })
        })
        .collect()
}
