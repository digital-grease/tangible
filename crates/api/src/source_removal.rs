// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Removing an import's source once the library has it
//! (docs/plans/source-retention.md, ADR 0017).
//!
//! An import from a download client or S3 whose integration says
//! `after_import: remove` gets a removal as it completes. The remover takes
//! each in turn, with a lease, and never holds a transaction across a call
//! to anything:
//!
//! - first it asks for an integrity check of the artifact, and waits for it.
//!   The import's own hashes say what staging held; the check says the
//!   library still holds it, which is what makes the source redundant. A
//!   check that finds anything wrong ends the removal, and nothing is ever
//!   removed for that import;
//! - then it removes at the source, having looked first that the source is
//!   still what was imported: a torrent still in Tangible's category at the
//!   path imported, an object of the size and ETag imported, a Usenet
//!   download still at the path imported. Anything else is left where it
//!   is, saying why.
//!
//! qBittorrent and S3 remove their own. SABnzbd and NZBGet cannot remove a
//! completed download's files, so Tangible deletes them itself (ADR 0017),
//! only in a watched root the host's operator marked removable, and only
//! the files whose bytes the library holds; then it asks the client to
//! forget the job.
//!
//! A source that cannot be reached is tried again with a backoff, saying
//! why on the import, for as long as the integration still removes. Turning
//! the setting off ends every removal not yet made.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tangible_db::integrations::{IntegrationRecord, get_integration, record_integration_health};
use tangible_db::integrity::{Requested, get_check, request_check};
use tangible_db::source_removals::{
    Ending, Hold, SourceRemovalRecord, artifact_contents, begin_removing, claim_next_removal,
    end_removal, hold_removal,
};
use tangible_db::{Database, DbError};
use tangible_domain::{IntegrityCheckId, IntegrityCheckState, SourceRemovalState};
use tangible_integrations::download::{ClientError, Phase, relative_to};
use tangible_integrations::outbound::OutboundClient;
use tangible_integrations::s3::{Deleted, ObjectError};
use tangible_integrations::settings::AfterImport;
use tangible_integrations::torrent::Infohash;
use tangible_storage::{WatchRootError, WatchRoots};
use tokio::sync::Notify;

use crate::download_clients::{DownloadClient, Folders, open_client, open_s3};
use crate::integrity::Integrity;
use crate::secrets::SecretKey;

/// How the remover behaves.
#[derive(Debug, Clone)]
pub struct SourceRemovalSettings {
    /// What to call this process in the lease.
    pub owner: String,
    /// How long a claim is held: longer than any one removal takes.
    pub lease: Duration,
    /// How long to wait when nothing is due and nobody nudges.
    pub poll_interval: Duration,
    /// How often a check still under way is looked at.
    pub check_poll: Duration,
    /// The first wait after a failed try at the source; it doubles.
    pub first_retry: Duration,
    /// The longest wait between tries.
    pub max_retry: Duration,
}

impl Default for SourceRemovalSettings {
    fn default() -> Self {
        Self {
            owner: "tangible-server".to_owned(),
            lease: Duration::from_mins(5),
            poll_interval: Duration::from_secs(60),
            check_poll: Duration::from_secs(15),
            first_retry: Duration::from_secs(60),
            max_retry: Duration::from_hours(1),
        }
    }
}

/// What the import runner holds to wake the remover when an import
/// completes.
#[derive(Debug, Clone, Default)]
pub struct SourceRemovalHandle {
    nudge: Arc<Notify>,
}

impl SourceRemovalHandle {
    /// A handle with nothing waiting on it yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wake the remover now rather than at its next poll.
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }
}

/// Removes sources.
#[derive(Debug, Clone)]
pub struct SourceRemover {
    database: Database,
    integrity: Integrity,
    http: Option<OutboundClient>,
    key: Option<Arc<SecretKey>>,
    handle: SourceRemovalHandle,
    settings: SourceRemovalSettings,
    roots: WatchRoots,
}

/// How one try at a removal came out.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    /// Removed, in words, with what was removed for the audit.
    Removed {
        detail: String,
        files: Option<i32>,
        bytes: Option<i64>,
        kept: Option<i32>,
        what: Value,
    },
    /// Gone before Tangible got to it.
    AlreadyGone(String),
    /// Left where it is, and why.
    Kept(String),
    /// Not now, and why; tried again after the backoff.
    Retry(String),
}

/// Text a client reported, made safe to show: no control characters, and
/// short.
fn shown(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// "1 file", "2 files".
fn files(count: usize) -> String {
    if count == 1 {
        "1 file".to_owned()
    } else {
        format!("{count} files")
    }
}

/// The wait before the next try, after `attempts` tries that failed.
fn backoff(settings: &SourceRemovalSettings, attempts: i32) -> Duration {
    let doublings = u32::try_from(attempts.clamp(0, 20)).unwrap_or(0);
    settings
        .first_retry
        .saturating_mul(2_u32.saturating_pow(doublings))
        .min(settings.max_retry)
}

fn seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

/// What an integration says becomes of a source after import.
fn after_import(record: &IntegrationRecord) -> AfterImport {
    record
        .configuration
        .get("after_import")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

fn descriptor_text(record: &SourceRemovalRecord, key: &str) -> Option<String> {
    record
        .source_descriptor
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

impl SourceRemover {
    /// Assemble a remover.
    #[must_use]
    pub fn new(
        database: Database,
        integrity: Integrity,
        http: Option<OutboundClient>,
        key: Option<Arc<SecretKey>>,
        handle: SourceRemovalHandle,
        settings: SourceRemovalSettings,
    ) -> Self {
        Self {
            database,
            integrity,
            http,
            key,
            handle,
            settings,
            roots: WatchRoots::default(),
        }
    }

    /// The same remover, deleting Usenet downloads in `roots`: those of
    /// them marked removable. Without, every such download is kept.
    #[must_use]
    pub fn with_roots(mut self, roots: WatchRoots) -> Self {
        self.roots = roots;
        self
    }

    /// Run removals until `shutdown` resolves.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        crate::background::run_steps(
            shutdown,
            || async {
                match self.step().await {
                    Ok(worked) => worked,
                    Err(error) => {
                        tracing::error!(error = ?error, "the source removals could not be read");
                        false
                    }
                }
            },
            || async {
                tokio::select! {
                    () = self.handle.nudge.notified() => {}
                    () = tokio::time::sleep(self.settings.poll_interval) => {}
                }
            },
        )
        .await;
        tracing::info!("source remover stopped");
    }

    /// Take one removal that is due and move it on. Returns whether there
    /// was one.
    ///
    /// # Errors
    ///
    /// [`DbError`] if the removals cannot be read or written.
    pub async fn step(&self) -> Result<bool, DbError> {
        let Some(removal) = claim_next_removal(
            self.database.pool(),
            &self.settings.owner,
            seconds(self.settings.lease),
        )
        .await?
        else {
            return Ok(false);
        };
        match removal.state {
            SourceRemovalState::Verifying => self.verify(&removal).await?,
            SourceRemovalState::Removing => self.remove(&removal).await?,
            // Only open removals are claimed.
            _ => {}
        }
        Ok(true)
    }

    /// Ask for the check of the library's copy, or see what it found.
    async fn verify(&self, removal: &SourceRemovalRecord) -> Result<(), DbError> {
        let pool = self.database.pool();
        let owner = &self.settings.owner;
        let wait = seconds(self.settings.check_poll);
        let Some(check_id) = removal.check_id else {
            return match request_check(pool, Some(removal.artifact_id), "source-removal").await? {
                Requested::Queued(check) | Requested::AlreadyActive(check) => {
                    hold_removal(
                        pool,
                        removal.id,
                        owner,
                        Hold {
                            check: Some(Some(*check.id.as_uuid())),
                            detail: Some(Some(
                                "Checking the library's copy before removing the source",
                            )),
                            failed_try: false,
                            wait_seconds: wait,
                        },
                    )
                    .await?;
                    self.integrity.nudge();
                    Ok(())
                }
                Requested::NoSuchArtifact | Requested::Deleted => {
                    self.finish(
                        removal,
                        Outcome::Kept("The artifact this import made is gone".to_owned()),
                    )
                    .await
                }
            };
        };
        match get_check(pool, IntegrityCheckId::from_uuid(check_id)).await? {
            Some(check) if !check.state.is_terminal() => {
                hold_removal(
                    pool,
                    removal.id,
                    owner,
                    Hold {
                        wait_seconds: wait,
                        ..Hold::default()
                    },
                )
                .await?;
            }
            Some(check) if check.state == IntegrityCheckState::Complete => {
                if check.problem_count > 0 {
                    self.refuse(removal, check.problem_count).await?;
                } else if begin_removing(pool, removal.id, owner).await? {
                    let mut removing = removal.clone();
                    removing.state = SourceRemovalState::Removing;
                    removing.attempts = 0;
                    self.remove(&removing).await?;
                }
            }
            // Failed, canceled or gone: ask again, after a while.
            _ => {
                hold_removal(
                    pool,
                    removal.id,
                    owner,
                    Hold {
                        check: Some(None),
                        detail: Some(Some(
                            "The check of the library's copy did not finish; it will be asked \
                             for again",
                        )),
                        failed_try: true,
                        wait_seconds: seconds(backoff(&self.settings, removal.attempts)),
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    /// End a removal whose copy failed its check: nothing is removed.
    async fn refuse(&self, removal: &SourceRemovalRecord, problems: i32) -> Result<(), DbError> {
        let ended = end_removal(
            self.database.pool(),
            removal.id,
            &self.settings.owner,
            &Ending {
                state: SourceRemovalState::Refused,
                detail: &format!(
                    "The library's copy failed its integrity check ({problems} {}), so nothing \
                     was removed",
                    if problems == 1 { "problem" } else { "problems" }
                ),
                files_removed: None,
                bytes_removed: None,
                files_kept: None,
                removed: None,
            },
        )
        .await?;
        if ended {
            tracing::warn!(
                removal_id = %removal.id,
                import_id = %removal.import_job_id,
                "the library's copy failed its check; the source is kept"
            );
        }
        Ok(())
    }

    /// Remove at the source, if the integration still says to.
    async fn remove(&self, removal: &SourceRemovalRecord) -> Result<(), DbError> {
        let pool = self.database.pool();
        let Some(record) = get_integration(pool, removal.integration_id).await? else {
            return self
                .finish(
                    removal,
                    Outcome::Kept("The integration it came from is gone".to_owned()),
                )
                .await;
        };
        if after_import(&record) != AfterImport::Remove {
            return self
                .finish(
                    removal,
                    Outcome::Kept(format!(
                        "{} no longer removes sources after import, so this one was left",
                        record.name
                    )),
                )
                .await;
        }
        let outcome = match (record.kind.as_str(), removal.source_kind.as_str()) {
            ("qbittorrent", "qbittorrent_job") => self.remove_torrent(removal, &record).await,
            ("s3", "s3_object") => self.remove_object(removal, &record).await,
            ("sabnzbd", "sabnzbd_job") | ("nzbget", "nzbget_job") => {
                self.remove_download(removal, &record).await
            }
            _ => Outcome::Kept(format!(
                "Tangible does not remove imports from {} after import",
                record.kind
            )),
        };
        self.finish(removal, outcome).await
    }

    /// Note how the client answered on the integration.
    async fn note(&self, record: &IntegrationRecord, outcome: Result<(), &ClientError>) {
        let health = outcome.map(|()| None).map_err(ClientError::code);
        let _ = record_integration_health(self.database.pool(), record.id, health).await;
    }

    /// Remove a torrent, with its files, from qBittorrent.
    async fn remove_torrent(
        &self,
        removal: &SourceRemovalRecord,
        record: &IntegrationRecord,
    ) -> Outcome {
        let Some(hash) = removal.client_job_id.as_deref().and_then(Infohash::parse) else {
            return Outcome::Kept("No torrent is recorded for this import".to_owned());
        };
        let Some(imported) = removal.content_path.as_deref() else {
            return Outcome::Kept(
                "Where the torrent was imported from was not recorded, so it was left".to_owned(),
            );
        };
        let opened = open_client(
            &self.database,
            self.http.as_ref(),
            self.key.as_deref(),
            record,
        )
        .await;
        let (mut client, folders) = match opened {
            Ok((DownloadClient::Qbittorrent(client), folders)) => (client, folders),
            Ok(_) => return Outcome::Kept(format!("{} is not a qBittorrent", record.name)),
            Err(error) => return Outcome::Retry(error.detail),
        };
        let torrent = match client.torrent(&hash).await {
            Ok(torrent) => torrent,
            Err(error) => {
                self.note(record, Err(&error)).await;
                return Outcome::Retry(error.to_string());
            }
        };
        self.note(record, Ok(())).await;
        let Some(torrent) = torrent else {
            return Outcome::AlreadyGone("qBittorrent no longer has the torrent".to_owned());
        };
        if torrent.category != folders.category {
            return Outcome::Kept(format!(
                "The torrent is in qBittorrent's category {:?} now, not {:?}, so it was left",
                shown(&torrent.category),
                folders.category
            ));
        }
        if relative_to(&folders.client_path, &torrent.content_path).as_deref() != Some(imported) {
            return Outcome::Kept(
                "qBittorrent has moved the torrent since it was imported, so it was left"
                    .to_owned(),
            );
        }
        match torrent.phase() {
            Phase::Complete { .. } => {}
            Phase::Failed(state) => {
                return Outcome::Kept(format!(
                    "qBittorrent reports the torrent as {}, so it was left",
                    shown(&state)
                ));
            }
            Phase::Downloading { .. } => {
                return Outcome::Retry(format!(
                    "Waiting for qBittorrent to finish with the torrent ({})",
                    shown(&torrent.state)
                ));
            }
        }
        if let Err(error) = client.remove(&hash, true).await {
            self.note(record, Err(&error)).await;
            return Outcome::Retry(error.to_string());
        }
        match client.torrent(&hash).await {
            Ok(None) => Outcome::Removed {
                detail: "Removed the torrent and its files from qBittorrent".to_owned(),
                files: None,
                bytes: Some(torrent.size).filter(|size| *size >= 0),
                kept: None,
                what: json!({
                    "kind": "torrent",
                    "infohash": hash.as_str(),
                    "with_files": true,
                }),
            },
            Ok(Some(_)) => {
                Outcome::Retry("qBittorrent still lists the torrent after removing it".to_owned())
            }
            Err(error) => {
                self.note(record, Err(&error)).await;
                Outcome::Retry(error.to_string())
            }
        }
    }

    /// Delete the object version imported, while it is still the one
    /// imported.
    async fn remove_object(
        &self,
        removal: &SourceRemovalRecord,
        record: &IntegrationRecord,
    ) -> Outcome {
        let (Some(bucket), Some(key)) = (
            descriptor_text(removal, "bucket"),
            descriptor_text(removal, "key"),
        ) else {
            return Outcome::Kept("The import does not say which object it was".to_owned());
        };
        let version = descriptor_text(removal, "version_id");
        let etag = descriptor_text(removal, "etag");
        let size = removal
            .source_descriptor
            .get("expected_bytes")
            .and_then(Value::as_u64);
        if version.is_none() && etag.is_none() {
            return Outcome::Kept(
                "Nothing pins the object to what was imported (no version or ETag), so it was \
                 left"
                    .to_owned(),
            );
        }
        let (client, settings) = match open_s3(
            &self.database,
            self.http.as_ref(),
            self.key.as_deref(),
            record,
        )
        .await
        {
            Ok(opened) => opened,
            Err(error) => return Outcome::Retry(error.detail),
        };
        if !settings.buckets.contains(&bucket) {
            return Outcome::Kept(format!(
                "{} no longer allows bucket {bucket}, so the object was left",
                record.name
            ));
        }
        let changed =
            || Outcome::Kept("The object has changed since it was imported, so it was left".into());
        let info = match client.head_version(&bucket, &key, version.as_deref()).await {
            Ok(info) => info,
            Err(ObjectError::Missing) => {
                self.note(record, Ok(())).await;
                return Outcome::AlreadyGone(format!("The object is no longer in bucket {bucket}"));
            }
            Err(ObjectError::Client(error)) => {
                self.note(record, Err(&error)).await;
                return Outcome::Retry(error.to_string());
            }
        };
        self.note(record, Ok(())).await;
        let etag_differs =
            matches!((&etag, &info.etag), (Some(imported), Some(now)) if imported != now);
        if size.is_some_and(|size| size != info.size) || etag_differs {
            return changed();
        }
        let mut answer = client
            .delete(&bucket, &key, version.as_deref(), etag.as_deref())
            .await;
        // A version is exact, so an endpoint that cannot delete only while
        // unchanged may delete it without asking.
        if matches!(answer, Ok(Deleted::Unconditional)) && version.is_some() {
            answer = client.delete(&bucket, &key, version.as_deref(), None).await;
        }
        let deleted = match answer {
            Ok(Deleted::Deleted) => {
                if version.is_some() {
                    format!("Deleted the imported version of the object from bucket {bucket}")
                } else {
                    format!("Deleted the object from bucket {bucket}")
                }
            }
            Ok(Deleted::Changed) => return changed(),
            Ok(Deleted::Unconditional) => {
                return Outcome::Kept(
                    "The endpoint cannot delete an object only while it is unchanged, and the \
                     bucket keeps no versions, so the object was left"
                        .to_owned(),
                );
            }
            Err(ObjectError::Missing) => {
                return Outcome::AlreadyGone(format!("Bucket {bucket} is no longer there"));
            }
            Err(ObjectError::Client(error)) => {
                self.note(record, Err(&error)).await;
                return Outcome::Retry(error.to_string());
            }
        };
        Outcome::Removed {
            detail: deleted,
            files: Some(1),
            bytes: i64::try_from(info.size).ok(),
            kept: None,
            what: json!({
                "kind": "s3_object",
                "bucket": bucket,
                "key": key,
                "version_id": version,
            }),
        }
    }

    /// Delete a Usenet download's files, those the library holds, and then
    /// have the client forget the job.
    async fn remove_download(
        &self,
        removal: &SourceRemovalRecord,
        record: &IntegrationRecord,
    ) -> Outcome {
        let Some(job_id) = removal.client_job_id.as_deref() else {
            return Outcome::Kept("No job is recorded for this import".to_owned());
        };
        let Some(imported) = removal.content_path.as_deref() else {
            return Outcome::Kept(
                "Where the download was imported from was not recorded, so it was left".to_owned(),
            );
        };
        let opened = open_client(
            &self.database,
            self.http.as_ref(),
            self.key.as_deref(),
            record,
        )
        .await;
        let (mut client, folders) = match opened {
            Ok(opened) => opened,
            Err(error) => return Outcome::Retry(error.detail),
        };
        let name = client.name();
        if !self.roots.is_removable(&folders.local_root) {
            return Outcome::Kept(format!(
                "The watched root {} is not one Tangible may delete in \
                 (TANGIBLE_REMOVABLE_ROOTS), so the download was left",
                folders.local_root
            ));
        }
        let job = match client.job(job_id).await {
            Ok(job) => {
                self.note(record, Ok(())).await;
                job
            }
            Err(error) => {
                self.note(record, Err(&error)).await;
                return Outcome::Retry(error.to_string());
            }
        };
        match job.as_ref().map(|job| &job.phase) {
            Some(Phase::Complete { content_path, .. })
                if relative_to(&folders.client_path, content_path).as_deref() != Some(imported) =>
            {
                return Outcome::Kept(format!(
                    "{name} has moved the download since it was imported, so it was left"
                ));
            }
            Some(Phase::Failed(state)) => {
                return Outcome::Kept(format!(
                    "{name} reports the download as {}, so it was left",
                    shown(state)
                ));
            }
            Some(Phase::Downloading { .. }) => {
                return Outcome::Retry(format!("Waiting for {name} to finish with the download"));
            }
            // Still where it was imported from; or forgotten there already,
            // and its files may still be here.
            Some(Phase::Complete { .. }) | None => {}
        }
        let forget = job.is_some().then_some(job_id);
        self.delete_download(removal, &mut client, &folders, imported, forget)
            .await
    }

    /// Delete the files at `imported` beneath the client's watched root that
    /// the library holds, then have the client forget `forget`, the job, if
    /// it still lists it.
    async fn delete_download(
        &self,
        removal: &SourceRemovalRecord,
        client: &mut DownloadClient,
        folders: &Folders,
        imported: &str,
        forget: Option<&str>,
    ) -> Outcome {
        let name = client.name();
        let library: HashSet<(String, u64)> =
            match artifact_contents(self.database.pool(), removal.artifact_id).await {
                Ok(rows) => rows
                    .into_iter()
                    .filter_map(|(digest, length)| {
                        u64::try_from(length).ok().map(|length| (digest, length))
                    })
                    .collect(),
                Err(error) => {
                    tracing::warn!(error = ?error, "could not read what the library holds");
                    return Outcome::Retry(
                        "What the library holds of the artifact could not be read".to_owned(),
                    );
                }
            };
        let removed = match self
            .roots
            .remove_matching(&folders.local_root, imported, &library)
            .await
        {
            Ok(removed) => removed,
            Err(WatchRootError::NotFound) => {
                if let Some(job_id) = forget {
                    let _ = client.forget(job_id).await;
                }
                return Outcome::AlreadyGone(format!(
                    "The download's files are no longer in {name}'s folder"
                ));
            }
            Err(WatchRootError::Unreadable) => {
                return Outcome::Retry(format!(
                    "{name}'s folder could not be read or written here; is the watched root \
                     {} mounted read-write?",
                    folders.local_root
                ));
            }
            Err(error) => return Outcome::Kept(format!("{error}, so the download was left")),
        };
        if removed.files_removed == 0 {
            return Outcome::Kept(format!(
                "Nothing in the download is what the library holds, so nothing was deleted \
                 ({} left)",
                files(removed.files_kept)
            ));
        }
        let left = if removed.files_kept > 0 {
            format!(
                ", and left {} the library does not hold",
                files(removed.files_kept)
            )
        } else {
            String::new()
        };
        let forgotten = match forget {
            None => String::new(),
            Some(job_id) => match client.forget(job_id).await {
                Ok(_) => format!("; {name} no longer lists the job"),
                Err(error) => format!("; {name} could not be asked to forget the job ({error})"),
            },
        };
        let detail = format!(
            "Deleted {} from {name}'s folder{left}{forgotten}",
            files(removed.files_removed)
        );
        Outcome::Removed {
            detail,
            files: Some(i32::try_from(removed.files_removed).unwrap_or(i32::MAX)),
            bytes: i64::try_from(removed.bytes_removed).ok(),
            kept: Some(i32::try_from(removed.files_kept).unwrap_or(i32::MAX)),
            what: json!({
                "kind": "usenet_download",
                "client": name,
                "job_id": removal.client_job_id,
                "files_removed": removed.files_removed,
                "files_kept": removed.files_kept,
            }),
        }
    }

    /// Record how a try came out.
    async fn finish(&self, removal: &SourceRemovalRecord, outcome: Outcome) -> Result<(), DbError> {
        let pool = self.database.pool();
        let owner = &self.settings.owner;
        let (state, detail, counts, what) = match outcome {
            Outcome::Retry(why) => {
                tracing::info!(removal_id = %removal.id, why, "a source removal will be tried again");
                hold_removal(
                    pool,
                    removal.id,
                    owner,
                    Hold {
                        check: None,
                        detail: Some(Some(&why)),
                        failed_try: true,
                        wait_seconds: seconds(backoff(&self.settings, removal.attempts)),
                    },
                )
                .await?;
                return Ok(());
            }
            Outcome::Removed {
                detail,
                files,
                bytes,
                kept,
                what,
            } => (
                SourceRemovalState::Removed,
                detail,
                (files, bytes, kept),
                Some(what),
            ),
            Outcome::AlreadyGone(why) => (
                SourceRemovalState::AlreadyGone,
                why,
                (None, None, None),
                None,
            ),
            Outcome::Kept(why) => (SourceRemovalState::Kept, why, (None, None, None), None),
        };
        let (files, bytes, kept) = counts;
        let ended = end_removal(
            pool,
            removal.id,
            owner,
            &Ending {
                state,
                detail: &detail,
                files_removed: files,
                bytes_removed: bytes,
                files_kept: kept,
                removed: what,
            },
        )
        .await?;
        if ended {
            tracing::info!(
                removal_id = %removal.id,
                import_id = %removal.import_job_id,
                state = %state,
                detail,
                "a source removal ended"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_from_the_first_to_the_longest() {
        let settings = SourceRemovalSettings::default();
        let waits: Vec<u64> = (0..9)
            .map(|attempts| backoff(&settings, attempts).as_secs())
            .collect();
        assert_eq!(waits, [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]);
        assert_eq!(backoff(&settings, -3).as_secs(), 60);
        assert_eq!(backoff(&settings, i32::MAX).as_secs(), 3600);
    }

    #[test]
    fn what_a_client_reports_is_shown_short_and_without_controls() {
        assert_eq!(shown("tv\nshows\u{1b}[31m"), "tvshows[31m");
        assert_eq!(shown(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn only_remove_said_by_name_removes() {
        let record = |configuration: Value| IntegrationRecord {
            id: uuid::Uuid::nil(),
            kind: "qbittorrent".to_owned(),
            name: "q".to_owned(),
            enabled: true,
            configuration,
            status: "ok".to_owned(),
            last_success_at: None,
            last_error_code: None,
            client_version: None,
            secret_fields: Vec::new(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(
            after_import(&record(json!({ "after_import": "remove" }))),
            AfterImport::Remove
        );
        for kept in [
            json!({}),
            json!({ "after_import": "keep" }),
            json!({ "after_import": "Remove" }),
            json!({ "after_import": true }),
        ] {
            assert_eq!(
                after_import(&record(kept.clone())),
                AfterImport::Keep,
                "{kept}"
            );
        }
    }
}
