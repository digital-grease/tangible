// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The garbage collection planner (docs/plans/storage-integrity.md, phase
//! S2): mark and sweep, with the sweep left out. It says which stored
//! objects nothing refers to and what collecting them would free; nothing
//! here, or anywhere yet, removes one.
//!
//! The tree is walked first and the marks read after, so whatever comes to
//! refer to a listed object has done so by the time it is looked for. Live
//! is anything the catalogue holds (components, primary objects, a positive
//! reference count, a retention lock), anything a published manifest names
//! (an artifact written and not yet catalogued), anything an import not yet
//! ended has promoted, and anything younger than the grace period, which
//! covers work that has promoted an object and not yet said so anywhere.

use std::collections::HashSet;
use std::time::Duration;

use serde::Serialize;
use tangible_db::Database;
use tangible_db::gc::{CatalogueMarks, catalogue_marks};
use tangible_domain::Sha256Digest;
use tangible_storage::{Inventory, ManifestStore, ManifestStoreError, StorageError};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::import::ImportCheckpoint;

/// The grace period unless asked for another, in days.
pub const DEFAULT_GRACE_DAYS: u32 = 7;

/// The longest grace period accepted, in days.
pub const MAX_GRACE_DAYS: u32 = 3650;

/// What a plan is made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcSettings {
    /// An unreferenced object younger than this is kept.
    pub grace: Duration,
    /// How many candidates and missing objects the plan names; it counts
    /// them all.
    pub keep: usize,
}

impl GcSettings {
    /// A grace period in whole days, and how many to name.
    #[must_use]
    pub fn days(grace_days: u32, keep: usize) -> Self {
        Self {
            grace: Duration::from_secs(u64::from(grace_days) * 86_400),
            keep,
        }
    }
}

/// How many, and how many bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct Tally {
    /// How many.
    pub count: u64,
    /// Their bytes.
    pub bytes: u64,
}

impl Tally {
    fn add(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }
}

/// An object collection would remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct GcCandidate {
    /// Its digest.
    pub sha256: String,
    /// Its length.
    pub size_bytes: u64,
    /// When its file was last written.
    pub modified_at: String,
    /// Whether the catalogue records it (unreferenced, unlocked). One it
    /// does not was promoted by work that never catalogued it.
    pub recorded: bool,
}

/// What garbage collection would do now. A dry run: nothing is removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct GcPlan {
    /// When the plan was made.
    pub planned_at: String,
    /// How young an unreferenced object is kept anyway, in seconds.
    pub grace_seconds: u64,
    /// Every object in the store.
    pub stored: Tally,
    /// Objects something refers to.
    pub live: Tally,
    /// Objects nothing refers to, younger than the grace period: kept.
    pub recent: Tally,
    /// Objects nothing refers to, older than the grace period: what
    /// collection would remove and free.
    pub collectable: Tally,
    /// The largest of them, first.
    pub candidates: Vec<GcCandidate>,
    /// Temporary files ingests that did not finish left behind.
    pub partial_files: Tally,
    /// How many of those are older than the grace period.
    pub stale_partial_files: u64,
    /// Entries in the object tree that are not objects. Never collected.
    pub unrecognized: Tally,
    /// Objects the catalogue records whose file is absent, with the sizes
    /// it recorded. A scrub says which artifacts they belong to.
    pub missing: Tally,
    /// Their digests, in order.
    pub missing_digests: Vec<String>,
    /// Published manifests that could not be read.
    pub manifests_unreadable: u64,
    /// Checkpoints of imports under way that could not be read.
    pub checkpoints_unreadable: u64,
    /// Whether everything that could refer to an object was read. When it
    /// was not, something unread may name a candidate, and collection
    /// would remove nothing.
    pub certain: bool,
}

/// Why a plan could not be made.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    /// The catalogue could not be read.
    #[error("the catalogue could not be read")]
    Database(#[from] tangible_db::DbError),
    /// The object tree could not be walked.
    #[error("the object tree could not be walked")]
    Storage(#[from] StorageError),
    /// The manifests could not be listed.
    #[error("the manifests could not be listed")]
    Manifests(#[from] ManifestStoreError),
}

/// Everything found to refer to an object, and what could not be read.
#[derive(Debug, Clone, Default)]
struct Marks {
    live: HashSet<Sha256Digest>,
    recorded: Vec<(Sha256Digest, i64)>,
    manifests_unreadable: u64,
    checkpoints_unreadable: u64,
}

/// Make a plan.
///
/// # Errors
///
/// [`GcError`] when the tree, the catalogue or the manifest list cannot be
/// read. A single manifest or checkpoint that cannot be read does not fail
/// the plan; it makes it uncertain.
pub async fn plan(
    database: &Database,
    manifests: &ManifestStore,
    settings: GcSettings,
) -> Result<GcPlan, GcError> {
    let inventory = manifests.objects().inventory().await?;
    let marks = marks(database, manifests).await?;
    Ok(assess(
        &inventory,
        &marks,
        OffsetDateTime::now_utc(),
        settings,
    ))
}

/// What refers to stored objects, for collection (ADR 0020): the objects
/// marked live, and whether every mark could be read. When one could not,
/// something unread may name an object, and nothing is to be collected.
///
/// # Errors
///
/// As [`plan`].
pub async fn live_objects(
    database: &Database,
    manifests: &ManifestStore,
) -> Result<(HashSet<Sha256Digest>, bool), GcError> {
    let marks = marks(database, manifests).await?;
    let certain = marks.manifests_unreadable == 0 && marks.checkpoints_unreadable == 0;
    Ok((marks.live, certain))
}

/// Read the marks: the catalogue's, then the manifests'. The catalogue
/// first: an import that completes between the two has published its
/// manifest before it ended.
async fn marks(database: &Database, manifests: &ManifestStore) -> Result<Marks, GcError> {
    let catalogue = catalogue_marks(database.pool()).await?;
    let deleted = catalogue.deleted.clone();
    let mut marks = from_catalogue(catalogue);
    for id in manifests.list().await? {
        // A deleted artifact's manifest is kept as its record; it uses
        // nothing (ADR 0020).
        if deleted.contains(&id) {
            continue;
        }
        match manifests.read(id).await {
            Ok(manifest) => marks.live.extend(
                manifest
                    .components
                    .iter()
                    .map(|component| component.content.sha256),
            ),
            // Gone since it was listed: it names nothing now.
            Err(ManifestStoreError::NotFound { .. }) => {}
            Err(error) => {
                tracing::warn!(artifact_id = %id, error = ?error, "a manifest could not be read while planning collection");
                marks.manifests_unreadable += 1;
            }
        }
    }
    Ok(marks)
}

fn from_catalogue(catalogue: CatalogueMarks) -> Marks {
    let mut marks = Marks {
        live: catalogue.live,
        recorded: catalogue.recorded,
        ..Marks::default()
    };
    for value in catalogue.import_checkpoints {
        match serde_json::from_value::<ImportCheckpoint>(value) {
            Ok(checkpoint) => marks
                .live
                .extend(checkpoint.promoted.values().map(|promoted| promoted.digest)),
            Err(_) => marks.checkpoints_unreadable += 1,
        }
    }
    marks
}

/// Sort the inventory by the marks.
fn assess(
    inventory: &Inventory,
    marks: &Marks,
    now: OffsetDateTime,
    settings: GcSettings,
) -> GcPlan {
    let grace = time::Duration::try_from(settings.grace).unwrap_or(time::Duration::MAX);
    // An object written in the future, by a clock that disagrees, is young.
    let young = |modified: std::time::SystemTime| now - OffsetDateTime::from(modified) < grace;
    let recorded: HashSet<Sha256Digest> = marks.recorded.iter().map(|(d, _)| *d).collect();

    let mut plan = GcPlan {
        planned_at: rfc3339(now),
        grace_seconds: settings.grace.as_secs(),
        stored: Tally::default(),
        live: Tally::default(),
        recent: Tally::default(),
        collectable: Tally::default(),
        candidates: Vec::new(),
        partial_files: Tally::default(),
        stale_partial_files: 0,
        unrecognized: Tally {
            count: inventory.unrecognized_count,
            bytes: inventory.unrecognized_bytes,
        },
        missing: Tally::default(),
        missing_digests: Vec::new(),
        manifests_unreadable: marks.manifests_unreadable,
        checkpoints_unreadable: marks.checkpoints_unreadable,
        certain: marks.manifests_unreadable == 0 && marks.checkpoints_unreadable == 0,
    };

    let mut candidates = Vec::new();
    for object in &inventory.objects {
        plan.stored.add(object.size_bytes);
        if marks.live.contains(&object.digest) {
            plan.live.add(object.size_bytes);
        } else if young(object.modified) {
            plan.recent.add(object.size_bytes);
        } else {
            plan.collectable.add(object.size_bytes);
            candidates.push(object);
        }
    }
    candidates.sort_unstable_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then(a.digest.cmp(&b.digest))
    });
    plan.candidates = candidates
        .into_iter()
        .take(settings.keep)
        .map(|object| GcCandidate {
            sha256: object.digest.to_hex(),
            size_bytes: object.size_bytes,
            modified_at: rfc3339(OffsetDateTime::from(object.modified)),
            recorded: recorded.contains(&object.digest),
        })
        .collect();

    for partial in &inventory.partial_files {
        plan.partial_files.add(partial.size_bytes);
        if !young(partial.modified) {
            plan.stale_partial_files += 1;
        }
    }

    // The inventory is in digest order, so presence is a binary search.
    let mut missing: Vec<&(Sha256Digest, i64)> = marks
        .recorded
        .iter()
        .filter(|(digest, _)| {
            inventory
                .objects
                .binary_search_by(|object| object.digest.cmp(digest))
                .is_err()
        })
        .collect();
    missing.sort_unstable_by_key(|(digest, _)| *digest);
    for (_, size) in &missing {
        plan.missing.add(u64::try_from(*size).unwrap_or(0));
    }
    plan.missing_digests = missing
        .into_iter()
        .take(settings.keep)
        .map(|(digest, _)| digest.to_hex())
        .collect();
    plan
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::SystemTime;

    use serde_json::json;
    use tangible_storage::{PartialFile, StoredObject};

    use super::*;
    use crate::import::PromotedComponent;

    const DAY: Duration = Duration::from_secs(86_400);

    fn digest(n: u8) -> Sha256Digest {
        Sha256Digest::from_bytes([n; 32])
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn days_ago(days: u32) -> SystemTime {
        SystemTime::from(now()) - DAY * days
    }

    fn object(n: u8, size: u64, modified: SystemTime) -> StoredObject {
        StoredObject {
            digest: digest(n),
            size_bytes: size,
            modified,
        }
    }

    fn inventory(mut objects: Vec<StoredObject>) -> Inventory {
        objects.sort_unstable_by_key(|object| object.digest);
        Inventory {
            objects,
            ..Inventory::default()
        }
    }

    #[test]
    fn live_young_and_old_unreferenced_objects_are_told_apart() {
        let inventory = inventory(vec![
            object(1, 100, days_ago(30)),
            object(2, 200, days_ago(30)),
            object(3, 300, days_ago(1)),
            object(4, 400, days_ago(10)),
        ]);
        let marks = Marks {
            live: HashSet::from([digest(1)]),
            recorded: vec![(digest(1), 100), (digest(4), 400)],
            ..Marks::default()
        };
        let plan = assess(&inventory, &marks, now(), GcSettings::days(7, 10));
        assert_eq!(
            plan.stored,
            Tally {
                count: 4,
                bytes: 1000
            }
        );
        assert_eq!(
            plan.live,
            Tally {
                count: 1,
                bytes: 100
            }
        );
        assert_eq!(
            plan.recent,
            Tally {
                count: 1,
                bytes: 300
            }
        );
        assert_eq!(
            plan.collectable,
            Tally {
                count: 2,
                bytes: 600
            }
        );
        // Largest first; the recorded one says so.
        let named: Vec<_> = plan
            .candidates
            .iter()
            .map(|c| (c.sha256.clone(), c.size_bytes, c.recorded))
            .collect();
        assert_eq!(
            named,
            vec![
                (digest(4).to_hex(), 400, true),
                (digest(2).to_hex(), 200, false),
            ]
        );
        assert_eq!(plan.grace_seconds, 7 * 86_400);
        assert!(plan.certain);
        assert_eq!(plan.missing, Tally::default());
    }

    #[test]
    fn candidates_are_bounded_and_counted_in_full() {
        let inventory = inventory((1..=5).map(|n| object(n, 10, days_ago(30))).collect());
        let plan = assess(&inventory, &Marks::default(), now(), GcSettings::days(7, 2));
        assert_eq!(plan.collectable.count, 5);
        assert_eq!(plan.candidates.len(), 2);
        // Equal sizes fall back to digest order.
        assert_eq!(plan.candidates[0].sha256, digest(1).to_hex());
    }

    #[test]
    fn no_grace_makes_every_unreferenced_object_collectable_but_the_future_stays() {
        let future = SystemTime::from(now()) + DAY;
        let inventory = inventory(vec![object(1, 1, days_ago(0)), object(2, 1, future)]);
        let plan = assess(
            &inventory,
            &Marks::default(),
            now(),
            GcSettings::days(0, 10),
        );
        assert_eq!(plan.collectable.count, 1);
        assert_eq!(plan.recent.count, 1);
    }

    #[test]
    fn recorded_objects_without_a_file_are_missing() {
        let inventory = inventory(vec![object(1, 10, days_ago(30))]);
        let marks = Marks {
            live: HashSet::from([digest(1), digest(9)]),
            recorded: vec![(digest(9), 90), (digest(1), 10), (digest(5), 50)],
            ..Marks::default()
        };
        let plan = assess(&inventory, &marks, now(), GcSettings::days(7, 10));
        assert_eq!(
            plan.missing,
            Tally {
                count: 2,
                bytes: 140
            }
        );
        assert_eq!(
            plan.missing_digests,
            vec![digest(5).to_hex(), digest(9).to_hex()]
        );
        assert_eq!(plan.collectable.count, 0);
    }

    #[test]
    fn partial_and_unrecognized_files_are_reported_never_collected() {
        let mut inventory = inventory(Vec::new());
        inventory.partial_files = vec![
            PartialFile {
                size_bytes: 5,
                modified: days_ago(30),
            },
            PartialFile {
                size_bytes: 7,
                modified: days_ago(0),
            },
        ];
        inventory.unrecognized_count = 3;
        inventory.unrecognized_bytes = 12;
        let plan = assess(
            &inventory,
            &Marks::default(),
            now(),
            GcSettings::days(7, 10),
        );
        assert_eq!(
            plan.partial_files,
            Tally {
                count: 2,
                bytes: 12
            }
        );
        assert_eq!(plan.stale_partial_files, 1);
        assert_eq!(
            plan.unrecognized,
            Tally {
                count: 3,
                bytes: 12
            }
        );
        assert_eq!(plan.collectable, Tally::default());
    }

    #[test]
    fn checkpoints_mark_what_they_promoted_and_an_unreadable_one_makes_the_plan_uncertain() {
        let catalogue = CatalogueMarks {
            live: HashSet::new(),
            import_checkpoints: vec![
                serde_json::to_value(ImportCheckpoint {
                    promoted: [(
                        "disc.iso".to_owned(),
                        PromotedComponent {
                            digest: digest(3),
                            length_bytes: 30,
                        },
                    )]
                    .into(),
                    ..ImportCheckpoint::default()
                })
                .unwrap(),
                json!({ "something": "else" }),
            ],
            recorded: Vec::new(),
            deleted: HashSet::new(),
        };
        let marks = from_catalogue(catalogue);
        assert!(marks.live.contains(&digest(3)));
        assert_eq!(marks.checkpoints_unreadable, 1);
        let inventory = inventory(vec![object(3, 30, days_ago(30))]);
        let plan = assess(&inventory, &marks, now(), GcSettings::days(7, 10));
        assert_eq!(plan.live.count, 1);
        assert!(!plan.certain);
    }
}
