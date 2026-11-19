// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Collecting orphaned objects (ADR 0024, docs/plans/gc-sweep.md G2).
//!
//! A plan is made and kept with every candidate; collecting it removes at
//! most those candidates, each only if it is still unreferenced and its file
//! is still the one planned. The deletion runner collects, holding the
//! store's collection lock for writing as it does for a deleted artifact's
//! objects, so no ingest is between storing bytes and registering them
//! meanwhile.
//!
//! An orphan is a copy: a failed import keeps its staging area, and a
//! retried import stores again whatever it stored that is gone (G1).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use tangible_db::Database;
use tangible_db::gc_plans::{
    self, Collected, CollectionSummary, NewPlan, NewPlanObject, Outcome, Refusal, RowHold,
};
use tangible_domain::Sha256Digest;
use tangible_storage::{FilesystemStore, ManifestStore};
use time::OffsetDateTime;

use crate::gc::{GcError, live_objects, plan_in_full};
use crate::import::ImportCheckpoint;

/// The shortest grace period a plan to be collected may have, in days.
pub const MIN_GRACE_DAYS: u32 = 1;

/// How long after it is made a plan can be collected.
pub const PLAN_LIFETIME: Duration = Duration::from_hours(1);

/// How long after a scheduled plan that could not read every mark the
/// schedule tries again, rather than waiting its whole interval.
pub const UNCERTAIN_RETRY: Duration = Duration::from_hours(24);

/// Whether the schedule makes a plan now (ADR 0024, decision 6): every
/// `interval_days` after the last collection ended, by hand or by
/// schedule, or at once when none has; never with an interval of 0, while
/// a plan waits to be collected, or within a day of a scheduled plan that
/// could not read every mark.
#[must_use]
pub fn scheduled_plan_due(
    interval_days: u32,
    summary: &CollectionSummary,
    now: OffsetDateTime,
) -> bool {
    if interval_days == 0 || summary.queued > 0 {
        return false;
    }
    let retry = time::Duration::try_from(UNCERTAIN_RETRY).unwrap_or(time::Duration::DAY);
    if let Some((made, false)) = summary.last_scheduled
        && now - made < retry
    {
        return false;
    }
    summary
        .last_collected
        .is_none_or(|last| now - last.at >= time::Duration::days(i64::from(interval_days)))
}

/// Make a plan as the schedule and ask for it to be collected, which the
/// deletion runner then does. The plan's id, and whether it was queued: one
/// that could not read every mark is kept and not queued.
///
/// # Errors
///
/// As [`make_plan`], or a database failure.
pub async fn schedule_plan(
    database: &Database,
    manifests: &ManifestStore,
) -> Result<(uuid::Uuid, bool), GcError> {
    let id = make_plan(database, manifests, crate::gc::DEFAULT_GRACE_DAYS, None).await?;
    let lifetime = time::Duration::try_from(PLAN_LIFETIME).unwrap_or(time::Duration::HOUR);
    let queued = gc_plans::request_collection(
        database.pool(),
        id,
        None,
        lifetime,
        i64::from(MIN_GRACE_DAYS) * 86_400,
    )
    .await?;
    match queued {
        Ok(_) => {
            tracing::info!(plan_id = %id, "scheduled orphan collection planned");
            Ok((id, true))
        }
        Err(Refusal::Uncertain) => {
            tracing::warn!(
                plan_id = %id,
                "a manifest or an import's checkpoint could not be read, so the scheduled \
                 orphan collection collects nothing; it tries again in a day"
            );
            Ok((id, false))
        }
        Err(refusal) => {
            tracing::warn!(plan_id = %id, ?refusal, "a scheduled plan was not queued");
            Ok((id, false))
        }
    }
}

/// Make a plan with grace period `grace_days` and keep it: `planned_by` is
/// the administrator, or `None` for the schedule. Its id.
///
/// # Errors
///
/// [`GcError`] when the tree, the catalogue or the manifest list cannot be
/// read, or the plan cannot be stored.
pub async fn make_plan(
    database: &Database,
    manifests: &ManifestStore,
    grace_days: u32,
    planned_by: Option<&str>,
) -> Result<uuid::Uuid, GcError> {
    let grace = Duration::from_secs(u64::from(grace_days) * 86_400);
    let full = plan_in_full(database, manifests, grace).await?;
    let origins = origins(database).await?;
    let objects: Vec<NewPlanObject> = full
        .candidates
        .iter()
        .map(|candidate| NewPlanObject {
            sha256: candidate.object.digest.to_hex(),
            size_bytes: whole(candidate.object.size_bytes),
            // A candidate is not young, so its time is a date
            // (crate::gc::young).
            modified_at: to_micros(candidate.object.modified).unwrap_or(OffsetDateTime::UNIX_EPOCH),
            recorded: candidate.recorded,
            import_job_id: origins.get(&candidate.object.digest).copied(),
        })
        .collect();
    let id = uuid::Uuid::now_v7();
    gc_plans::insert_plan(
        database.pool(),
        &NewPlan {
            id,
            planned_by: planned_by.map(str::to_owned),
            grace_seconds: whole(full.summary.grace_seconds),
            certain: full.summary.certain,
            candidate_count: whole(full.summary.collectable.count),
            candidate_bytes: whole(full.summary.collectable.bytes),
            stale_partial_files: whole(full.summary.stale_partial_files),
        },
        &objects,
    )
    .await?;
    Ok(id)
}

/// Which import each orphan came from: the newest ended import whose
/// checkpoint lists it. A checkpoint that cannot be read names nothing.
async fn origins(database: &Database) -> Result<HashMap<Sha256Digest, uuid::Uuid>, GcError> {
    let mut origins = HashMap::new();
    for (id, value) in gc_plans::orphaning_import_checkpoints(database.pool()).await? {
        if let Ok(checkpoint) = serde_json::from_value::<ImportCheckpoint>(value) {
            for promoted in checkpoint.promoted.values() {
                origins.entry(promoted.digest).or_insert(id);
            }
        }
    }
    Ok(origins)
}

/// Collect one plan asked to be collected: its candidates still
/// unreferenced and unchanged, and the stale partial files. `None` if it was
/// not asked for, or is collected already.
///
/// The caller holds the store's collection lock for writing.
///
/// # Errors
///
/// [`GcError`] when the catalogue or the marks cannot be read, or the
/// outcome cannot be recorded; the plan then stays queued for the next pass.
pub async fn collect_plan(
    database: &Database,
    manifests: &ManifestStore,
    plan_id: uuid::Uuid,
) -> Result<Option<Collected>, GcError> {
    let pool = database.pool();
    let Some(plan) = gc_plans::get_plan(pool, plan_id).await? else {
        return Ok(None);
    };
    if plan.collect_requested_at.is_none() || plan.collected.is_some() {
        return Ok(None);
    }
    let candidates = gc_plans::plan_candidates(pool, plan_id).await?;
    // Read again under the lock: what refers to an object now, not when the
    // plan was made.
    let (live, certain) = live_objects(database, manifests).await?;
    let objects = manifests.objects();
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            u64::try_from(plan.grace_seconds).unwrap_or(0),
        ))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let check = Check {
        database,
        objects,
        live: &live,
        certain,
        cutoff,
    };

    let mut totals = Collected {
        at: OffsetDateTime::now_utc(),
        removed_count: 0,
        removed_bytes: 0,
        kept_count: 0,
        kept_bytes: 0,
        partial_files_removed: 0,
    };
    let mut outcomes = Vec::with_capacity(candidates.len());
    for (sha256, size, modified_at, _) in candidates {
        let outcome = check.collect(&sha256, size, modified_at).await?;
        if outcome == Outcome::Removed {
            totals.removed_count += 1;
            totals.removed_bytes += size;
        } else {
            totals.kept_count += 1;
            totals.kept_bytes += size;
        }
        outcomes.push((sha256, outcome));
    }
    if certain {
        totals.partial_files_removed = remove_stale_partials(objects, cutoff).await;
    } else {
        tracing::warn!(
            plan_id = %plan_id,
            "a manifest or an import's checkpoint could not be read, so no orphan was removed"
        );
    }
    totals.at = OffsetDateTime::now_utc();
    if gc_plans::record_collection(pool, plan_id, &outcomes, totals).await? {
        tracing::info!(
            plan_id = %plan_id,
            removed = totals.removed_count,
            bytes_freed = totals.removed_bytes,
            kept = totals.kept_count,
            partial_files_removed = totals.partial_files_removed,
            "orphaned objects collected"
        );
        Ok(Some(totals))
    } else {
        Ok(None)
    }
}

/// What every candidate is checked against.
struct Check<'a> {
    database: &'a Database,
    objects: &'a FilesystemStore,
    live: &'a HashSet<Sha256Digest>,
    certain: bool,
    cutoff: SystemTime,
}

impl Check<'_> {
    /// Remove one candidate if it may still go.
    async fn collect(
        &self,
        sha256: &str,
        size: i64,
        modified_at: OffsetDateTime,
    ) -> Result<Outcome, GcError> {
        if !self.certain {
            return Ok(Outcome::KeptUncertain);
        }
        let Ok(digest) = sha256.parse::<Sha256Digest>() else {
            return Ok(Outcome::KeptChanged);
        };
        if self.live.contains(&digest) {
            return Ok(Outcome::KeptLive);
        }
        // The file must be the one planned: its length, its last write, and
        // still older than the grace period.
        let file = match self.objects.object_file(&digest).await {
            Ok(file) => file,
            Err(error) => {
                tracing::error!(%sha256, error = ?error, "an orphan could not be inspected; it is kept");
                return Ok(Outcome::KeptFailed);
            }
        };
        let Some((length, modified)) = file else {
            return Ok(Outcome::KeptGone);
        };
        if whole(length) != size
            || to_micros(modified) != Some(modified_at)
            || modified >= self.cutoff
        {
            return Ok(Outcome::KeptChanged);
        }
        match gc_plans::hold_row(self.database.pool(), sha256).await? {
            RowHold::Held => Ok(Outcome::KeptHeld),
            RowHold::Unrecorded => Ok(remove(self.objects, &digest).await),
            RowHold::Removable(removal) => {
                let outcome = remove(self.objects, &digest).await;
                if outcome == Outcome::Removed {
                    removal.finish().await?;
                } else {
                    removal.abandon().await?;
                }
                Ok(outcome)
            }
        }
    }
}

async fn remove(objects: &FilesystemStore, digest: &Sha256Digest) -> Outcome {
    match objects.remove(digest).await {
        Ok(true) => Outcome::Removed,
        Ok(false) => Outcome::KeptGone,
        Err(error) => {
            tracing::error!(sha256 = %digest, error = ?error, "an orphan could not be removed; it is kept");
            Outcome::KeptFailed
        }
    }
}

/// Remove the partial files in `incoming` last written before `cutoff`;
/// how many. A failure is logged and leaves that file.
async fn remove_stale_partials(objects: &FilesystemStore, cutoff: SystemTime) -> i64 {
    let partials = match objects.partial_files().await {
        Ok(partials) => partials,
        Err(error) => {
            tracing::error!(error = ?error, "the partial files could not be listed; none was removed");
            return 0;
        }
    };
    let mut removed = 0;
    for partial in partials.iter().filter(|partial| partial.modified < cutoff) {
        match objects.remove_stale_partial(&partial.name, cutoff).await {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::error!(error = ?error, "a partial file could not be removed; it is kept");
            }
        }
    }
    removed
}

/// A count or length as the database holds it.
fn whole(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// A file time to the microsecond, as PostgreSQL keeps a timestamp, so a
/// file's time can be compared with the one its plan recorded; `None` when
/// no date can hold it ([`crate::gc::file_time`]).
fn to_micros(at: SystemTime) -> Option<OffsetDateTime> {
    let at = crate::gc::file_time(at)?;
    Some(
        at.replace_nanosecond(at.nanosecond() / 1_000 * 1_000)
            .unwrap_or(at),
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn at(days: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap() + time::Duration::days(days)
    }

    fn collected(on: i64) -> Collected {
        Collected {
            at: at(on),
            removed_count: 1,
            removed_bytes: 1,
            kept_count: 0,
            kept_bytes: 0,
            partial_files_removed: 0,
        }
    }

    #[test]
    fn the_schedule_runs_every_interval_after_the_last_collection() {
        let never = CollectionSummary::default();
        // Off.
        assert!(!scheduled_plan_due(0, &never, at(0)));
        // Never collected: at once.
        assert!(scheduled_plan_due(7, &never, at(0)));
        // Collected six days ago, by hand or by schedule: not yet; seven: now.
        let last = CollectionSummary {
            last_collected: Some(collected(0)),
            ..CollectionSummary::default()
        };
        assert!(!scheduled_plan_due(7, &last, at(6)));
        assert!(scheduled_plan_due(7, &last, at(7)));
        assert!(!scheduled_plan_due(0, &last, at(100)));
    }

    #[test]
    fn the_schedule_waits_for_a_queued_plan_and_a_day_after_an_uncertain_one() {
        let queued = CollectionSummary {
            queued: 1,
            ..CollectionSummary::default()
        };
        assert!(!scheduled_plan_due(1, &queued, at(30)));
        let uncertain = CollectionSummary {
            last_collected: Some(collected(0)),
            last_scheduled: Some((at(10), false)),
            queued: 0,
        };
        // Due since day 7, but the plan on day 10 could not read every mark:
        // a day later, not seven.
        assert!(!scheduled_plan_due(
            7,
            &uncertain,
            at(10) + time::Duration::hours(23)
        ));
        assert!(scheduled_plan_due(7, &uncertain, at(11)));
        // A certain scheduled plan does not hold it back.
        let certain = CollectionSummary {
            last_scheduled: Some((at(10), true)),
            ..uncertain
        };
        assert!(scheduled_plan_due(7, &certain, at(10)));
    }

    #[test]
    fn a_file_time_keeps_its_microseconds_and_drops_the_rest() {
        let at = SystemTime::UNIX_EPOCH + Duration::new(1_800_000_000, 123_456_789);
        let micros = to_micros(at).unwrap();
        assert_eq!(micros.nanosecond(), 123_456_000);
        assert_eq!(micros.unix_timestamp(), 1_800_000_000);
        // Before 1970 as well: the whole second is not moved.
        let early = SystemTime::UNIX_EPOCH - Duration::new(10, 500_000_001);
        assert_eq!(to_micros(early).unwrap().nanosecond(), 499_999_000);
    }

    #[test]
    fn a_file_time_no_date_can_hold_is_none_not_a_panic() {
        // About the year 14645, which btrfs, ZFS or a restore tool can leave
        // on a file; `OffsetDateTime::from` panics on it.
        let far = SystemTime::UNIX_EPOCH + Duration::from_secs(400_000_000_000);
        assert_eq!(to_micros(far), None);
        let long_ago = SystemTime::UNIX_EPOCH - Duration::from_secs(400_000_000_000);
        assert_eq!(to_micros(long_ago), None);
    }

    #[test]
    fn counts_beyond_what_the_database_holds_are_capped() {
        assert_eq!(whole(5), 5);
        assert_eq!(whole(u64::MAX), i64::MAX);
    }
}
