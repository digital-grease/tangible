// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Keeping RomM in step with the export through its API
//! (docs/plans/romm-api.md M2).
//!
//! A background task beside the exporter. When the exporter writes, renames
//! or removes a game's folder, it records the platform folder as owed a
//! scan and nudges this task. Once RomM's API is configured, with
//! `scan_after_export` on and an instance that takes scans from a token
//! (RomM 5.4 and later), the task asks RomM for one quick scan of every
//! platform owed one and follows it to its end, or until it has waited 30
//! minutes. A platform stays owed until a scan asked for after its change
//! finishes, so a scan RomM refuses (one already running, or no scan worker
//! listening) or that fails is asked for again, sooner at first and then
//! hourly.
//!
//! Then it looks each export up among RomM's ROMs (`crate::romm_link`) and
//! records what it found: RomM's id for the ROM and whether its files agree
//! with what was written, or that RomM has not seen it, or marks it
//! missing. A pass prompted by the exporter looks up only what changed or
//! is not linked; one an hour looks up everything, so RomM's own scans are
//! noticed. A removed export is followed by its ROM's id until RomM marks
//! it missing. The platforms RomM knows are read on the hourly pass, for
//! the platform picker.
//!
//! Everything it knows is in the database, but for the platforms RomM
//! knows: what is owed, each scan asked for with RomM's task id, and each
//! link. A scan left open by a restart is followed again on the next pass.
//! No transaction is held across a request to RomM.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tangible_db::Database;
use tangible_db::integrations::{record_integration_health, romm_api_integration};
use tangible_db::romm::{LinkTarget, NewLink, exported_files, exports_to_link, record_link};
use tangible_db::romm_api::{
    NewScan, ScanCounts, ScanRecord, ScanState, open_scans, owed_scans, prune_scans, record_scan,
    settle_owed_scans, update_scan,
};
use tangible_integrations::download::ClientError;
use tangible_integrations::outbound::OutboundClient;
use tangible_integrations::romm_api::{RommClient, ScanAnswer, ScanStats, TaskPhase};
use time::OffsetDateTime;
use tokio::sync::Notify;

use crate::download_clients::open_romm;
use crate::romm_link::{Finder, Link, current_link, removed_link};
use crate::secrets::SecretKey;

/// How the task paces itself.
#[derive(Debug, Clone, Copy)]
pub struct RommSyncSettings {
    /// How often it looks when nothing prompts it sooner.
    pub interval: Duration,
    /// How often a scan is asked after while it runs.
    pub poll: Duration,
    /// How long a scan is followed before Tangible stops waiting.
    pub scan_bound: Duration,
    /// How soon a refused or failed scan is first asked for again; each
    /// further try waits twice as long, up to `interval`.
    pub retry: Duration,
    /// How many ended scans are kept on record.
    pub keep_scans: i64,
}

impl Default for RommSyncSettings {
    fn default() -> Self {
        Self {
            interval: Duration::from_hours(1),
            poll: Duration::from_secs(5),
            scan_bound: Duration::from_mins(30),
            retry: Duration::from_secs(60),
            keep_scans: 200,
        }
    }
}

/// The means to prompt the task, held by the exporter and the routes, and
/// what it last read of the platforms RomM knows.
#[derive(Debug, Clone, Default)]
pub struct RommSyncHandle {
    nudge: Arc<Notify>,
    known_platforms: Arc<RwLock<Option<Vec<String>>>>,
}

impl RommSyncHandle {
    /// A handle for a task not yet started.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the task to look again now rather than at its next interval.
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }

    /// The slugs of the platforms the connected RomM knows, as last read;
    /// `None` until they have been.
    #[must_use]
    pub fn known_platforms(&self) -> Option<Vec<String>> {
        self.known_platforms
            .read()
            .ok()
            .and_then(|known| known.clone())
    }

    fn set_known_platforms(&self, known: Option<Vec<String>>) {
        if let Ok(mut slot) = self.known_platforms.write() {
            *slot = known;
        }
    }
}

/// How one pass went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pass {
    /// The scan asked for in this pass, and where it ended.
    pub scan: Option<ScanState>,
    /// Whether platforms are still owed a scan RomM could be asked for
    /// again.
    pub retry_soon: bool,
    /// How many exports were looked up in RomM.
    pub looked_up: usize,
}

/// The background task.
#[derive(Debug, Clone)]
pub struct RommSync {
    database: Database,
    http: Option<OutboundClient>,
    key: Option<Arc<SecretKey>>,
    settings: RommSyncSettings,
    handle: RommSyncHandle,
}

impl RommSync {
    /// A task reading the catalog from `database`, reaching RomM through
    /// `http` (the integrations client), opening the token with `key`, and
    /// prompted through `handle`.
    #[must_use]
    pub fn new(
        database: Database,
        http: Option<OutboundClient>,
        key: Option<Arc<SecretKey>>,
        settings: RommSyncSettings,
        handle: RommSyncHandle,
    ) -> Self {
        Self {
            database,
            http,
            key,
            settings,
            handle,
        }
    }

    /// Run until `shutdown` resolves. A pass under way when it does is
    /// dropped: what it asked RomM is on record, and the next start follows
    /// it.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        let mut shutdown = std::pin::pin!(shutdown);
        let mut retry = self.settings.retry;
        let mut last_full: Option<Instant> = None;
        loop {
            let full = last_full.is_none_or(|at| at.elapsed() >= self.settings.interval);
            if full {
                last_full = Some(Instant::now());
            }
            let pass = tokio::select! {
                () = &mut shutdown => return,
                pass = self.run_pass(full) => pass,
            };
            let wait = if pass.retry_soon {
                let wait = retry;
                retry = (retry * 2).min(self.settings.interval);
                wait
            } else {
                retry = self.settings.retry;
                self.settings.interval
            };
            tokio::select! {
                () = &mut shutdown => return,
                () = self.handle.nudge.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// The hourly pass: follow scans left open, ask for one of what is
    /// owed, look up every export, and read the platforms RomM knows.
    pub async fn pass(&self) -> Pass {
        self.run_pass(true).await
    }

    /// A pass prompted by a change: as [`Self::pass`], looking up only the
    /// exports that changed or are not linked.
    pub async fn prompted_pass(&self) -> Pass {
        self.run_pass(false).await
    }

    async fn run_pass(&self, full: bool) -> Pass {
        match self.try_pass(full).await {
            Ok(pass) => pass,
            Err(error) => {
                tracing::error!(error = ?error, "could not keep RomM in step");
                Pass {
                    retry_soon: true,
                    ..Pass::default()
                }
            }
        }
    }

    async fn try_pass(&self, full: bool) -> Result<Pass, tangible_db::DbError> {
        let pool = self.database.pool();
        let Some(record) = romm_api_integration(pool).await? else {
            return Ok(Pass::default());
        };
        if !record.enabled {
            return Ok(Pass::default());
        }
        let (client, settings) = match open_romm(
            &self.database,
            self.http.as_ref(),
            self.key.as_deref(),
            &record,
        )
        .await
        {
            Ok(opened) => opened,
            Err(error) => {
                tracing::warn!(code = error.code, detail = %error.detail, "RomM's API could not be opened");
                record_integration_health(pool, record.id, Err(error.code)).await?;
                return Ok(Pass::default());
            }
        };
        if full || self.handle.known_platforms().is_none() {
            let known = match client.known_platforms().await {
                Ok(known) => Some(known),
                Err(error) => {
                    tracing::debug!(error = %error, "could not read the platforms RomM knows");
                    None
                }
            };
            self.handle.set_known_platforms(known);
        }

        for scan in open_scans(pool, record.id).await? {
            let ended = self.follow(&client, &scan).await?;
            if ended == ScanState::Finished {
                settle_owed_scans(pool, &scan.platforms, scan.requested_at).await?;
            }
        }

        let mut pass = Pass::default();
        if settings.scan_after_export {
            let owed = owed_scans(pool).await?;
            if !owed.is_empty() {
                let platforms: Vec<String> = owed.iter().map(|o| o.platform.clone()).collect();
                // Every change read was made by now; one made after is
                // owed again.
                let cutoff = owed
                    .iter()
                    .map(|o| o.owed_at)
                    .max()
                    .unwrap_or_else(OffsetDateTime::now_utc);
                let ended = self.scan(&client, record.id, &platforms).await?;
                if ended == Some(ScanState::Finished) {
                    settle_owed_scans(pool, &platforms, cutoff).await?;
                }
                pass.scan = ended;
                pass.retry_soon = ended.is_some_and(|state| state != ScanState::Finished);
            }
        }
        match self.link(&client, full).await? {
            Ok(looked_up) => {
                pass.looked_up = looked_up;
                if looked_up > 0 {
                    record_integration_health(pool, record.id, Ok(None)).await?;
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "could not look exports up in RomM");
                self.unhealthy(record.id, &error).await?;
            }
        }
        prune_scans(pool, record.id, self.settings.keep_scans).await?;
        Ok(pass)
    }

    /// Look the exports up in RomM and record what was found, answering how
    /// many were. A client error stops the pass: the next looks again.
    async fn link(
        &self,
        client: &RommClient,
        full: bool,
    ) -> Result<Result<usize, ClientError>, tangible_db::DbError> {
        let pool = self.database.pool();
        let targets = exports_to_link(pool, full).await?;
        let mut finder = Finder::new(client);
        let mut looked_up = 0;
        for target in &targets {
            let link = match self.look_up(&mut finder, client, target).await? {
                Ok(link) => link,
                Err(error) => return Ok(Err(error)),
            };
            let recorded = record_link(
                pool,
                target,
                link.as_ref().map(|link| NewLink {
                    state: link.state.as_str(),
                    rom_id: link.rom_id,
                    detail: link.detail.as_deref(),
                }),
            )
            .await?;
            if recorded {
                looked_up += 1;
            }
        }
        Ok(Ok(looked_up))
    }

    async fn look_up(
        &self,
        finder: &mut Finder<'_>,
        client: &RommClient,
        target: &LinkTarget,
    ) -> Result<Result<Option<Link>, ClientError>, tangible_db::DbError> {
        let Some(folder) = target
            .folder
            .as_deref()
            .filter(|_| target.state == "current")
        else {
            // Taken out: followed by its ROM's id until RomM marks it
            // missing, and cleared when RomM no longer has it.
            let Some(id) = target.rom_id else {
                return Ok(Ok(None));
            };
            return Ok(client.rom(id).await.map(|rom| removed_link(rom.as_ref())));
        };
        let found = match finder.find(folder, target.rom_id).await {
            Ok(found) => found,
            Err(error) => return Ok(Err(error)),
        };
        let exported = exported_files(self.database.pool(), folder).await?;
        Ok(Ok(Some(current_link(folder, &found, &exported))))
    }

    /// Ask for a quick scan of `platforms` and follow it. `None` when this
    /// RomM takes no scans from a token, so nothing was asked.
    async fn scan(
        &self,
        client: &RommClient,
        integration_id: uuid::Uuid,
        platforms: &[String],
    ) -> Result<Option<ScanState>, tangible_db::DbError> {
        let pool = self.database.pool();
        let answer = match client.capabilities().await {
            Ok(capabilities) if !capabilities.scan => {
                tracing::debug!("this RomM takes no scans from a token; scan it in RomM");
                return Ok(None);
            }
            Ok(_) => client.scan(platforms).await,
            Err(error) => Err(error),
        };
        let (detail, queued) = match answer {
            Ok(ScanAnswer::Queued(task)) => (None, Some(task)),
            Ok(ScanAnswer::Busy(detail)) => (Some(format!("RomM is busy: {detail}")), None),
            Ok(ScanAnswer::NoWorker(detail)) => (
                Some(format!(
                    "RomM has no scan worker: {detail}; restarting RomM brings it back"
                )),
                None,
            ),
            Err(error) => {
                self.unhealthy(integration_id, &error).await?;
                (Some(error.to_string()), None)
            }
        };
        let Some(task) = queued else {
            let detail = detail.unwrap_or_default();
            tracing::warn!(platforms = ?platforms, %detail, "RomM did not queue a scan; asking again later");
            record_scan(
                pool,
                NewScan {
                    integration_id,
                    platforms,
                    task_id: None,
                    state: ScanState::Refused,
                    detail: Some(&detail),
                    counts: None,
                },
            )
            .await?;
            return Ok(Some(ScanState::Refused));
        };

        record_integration_health(pool, integration_id, Ok(None)).await?;
        let state = state_of(task.phase);
        let counts = task.stats.map(counts_of);
        let id = record_scan(
            pool,
            NewScan {
                integration_id,
                platforms,
                task_id: Some(&task.id),
                state,
                detail: None,
                counts,
            },
        )
        .await?;
        tracing::info!(platforms = ?platforms, task = %task.id, "asked RomM for a quick scan");
        let scan = ScanRecord {
            id,
            integration_id,
            platforms: platforms.to_vec(),
            task_id: Some(task.id),
            state,
            detail: None,
            counts,
            requested_at: OffsetDateTime::now_utc(),
            ended_at: None,
        };
        self.follow(client, &scan).await.map(Some)
    }

    /// Follow an open scan until it ends, RomM no longer has it, or the
    /// bound since it was asked for passes, recording each change.
    async fn follow(
        &self,
        client: &RommClient,
        scan: &ScanRecord,
    ) -> Result<ScanState, tangible_db::DbError> {
        let pool = self.database.pool();
        let Some(task_id) = scan.task_id.as_deref() else {
            update_scan(pool, scan.id, ScanState::Lost, None, None).await?;
            return Ok(ScanState::Lost);
        };
        let deadline = scan.requested_at + self.settings.scan_bound;
        let mut state = scan.state;
        let mut counts = scan.counts;
        while state.open() {
            let now = OffsetDateTime::now_utc();
            if now >= deadline {
                let minutes = self.settings.scan_bound.as_secs() / 60;
                let detail = format!(
                    "it had not ended after {minutes} minutes, so Tangible stopped waiting; RomM \
                     may still finish it"
                );
                update_scan(pool, scan.id, ScanState::TimedOut, Some(&detail), None).await?;
                tracing::warn!(task = task_id, "a RomM scan had not ended in time");
                return Ok(ScanState::TimedOut);
            }
            let left = Duration::try_from(deadline - now).unwrap_or_default();
            tokio::time::sleep(self.settings.poll.min(left)).await;
            match client.task(task_id).await {
                Ok(Some(task)) => {
                    let next = state_of(task.phase);
                    let next_counts = task.stats.map(counts_of).or(counts);
                    if next != state || next_counts != counts {
                        let detail = match next {
                            ScanState::Failed => Some("RomM's scan failed; RomM's log says why"),
                            ScanState::Canceled => Some("the scan was stopped in RomM"),
                            _ => None,
                        };
                        update_scan(pool, scan.id, next, detail, next_counts).await?;
                    }
                    state = next;
                    counts = next_counts;
                }
                Ok(None) => {
                    update_scan(
                        pool,
                        scan.id,
                        ScanState::Lost,
                        Some("RomM no longer has the task, so how it ended is not known"),
                        None,
                    )
                    .await?;
                    return Ok(ScanState::Lost);
                }
                Err(error) => {
                    // RomM may be restarting; the bound decides.
                    tracing::warn!(task = task_id, error = %error, "could not ask RomM after a scan");
                }
            }
        }
        if state == ScanState::Finished {
            tracing::info!(task = task_id, "RomM finished a scan");
        }
        Ok(state)
    }

    async fn unhealthy(
        &self,
        integration_id: uuid::Uuid,
        error: &ClientError,
    ) -> Result<(), tangible_db::DbError> {
        record_integration_health(self.database.pool(), integration_id, Err(error.code())).await
    }
}

/// The record's word for where a task stands.
const fn state_of(phase: TaskPhase) -> ScanState {
    match phase {
        TaskPhase::Waiting => ScanState::Queued,
        TaskPhase::Running => ScanState::Running,
        TaskPhase::Finished => ScanState::Finished,
        TaskPhase::Failed => ScanState::Failed,
        TaskPhase::Canceled => ScanState::Canceled,
    }
}

const fn counts_of(stats: ScanStats) -> ScanCounts {
    ScanCounts {
        total_roms: stats.total_roms,
        scanned_roms: stats.scanned_roms,
        new_roms: stats.new_roms,
        updated_roms: stats.updated_roms,
    }
}
