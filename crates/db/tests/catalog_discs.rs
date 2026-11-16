// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Finding and cataloguing discs where a burn is queued, against PostgreSQL.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::PgPool;
use tangible_db::catalog_discs::{
    Catalogued, DiscSearch, NewCataloguedDisc, catalogue_disc, find_discs,
};
use tangible_db::{Database, DbConfig};
use tangible_domain::{ArtifactId, DiscId};

async fn pool() -> PgPool {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("TANGIBLE_TEST_DATABASE_URL must be set for integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    database.pool().clone()
}

/// An artifact row, enough to link.
async fn artifact(pool: &PgPool) -> ArtifactId {
    let id = ArtifactId::generate();
    sqlx::query(
        "INSERT INTO artifacts (id, origin, manifest_version, total_bytes, component_count,
             validation_state)
         VALUES ($1, 'imported_original', 'v1alpha1', 1, 1, 'valid')",
    )
    .bind(id.as_uuid())
    .execute(pool)
    .await
    .expect("an artifact");
    id
}

/// A name no other test's rows share.
fn unique(name: &str) -> String {
    format!("{name} {}", uuid::Uuid::now_v7().simple())
}

fn new<'a>(
    title: &'a str,
    edition: &'a str,
    artifact: Option<ArtifactId>,
) -> NewCataloguedDisc<'a> {
    NewCataloguedDisc {
        title,
        kind: "movie",
        release_year: Some(1995),
        edition,
        media_family: "dvd",
        artifact,
        actor: "admin",
    }
}

fn disc_of(outcome: Catalogued) -> DiscId {
    match outcome {
        Catalogued::Disc { disc, .. } => disc,
        Catalogued::NoArtifact => panic!("expected a disc"),
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_release_is_catalogued_in_one_step_linked_and_audited() {
    let pool = pool().await;
    let title = unique("Hackers");
    let artifact = artifact(&pool).await;
    let outcome = catalogue_disc(&pool, new(&title, "DVD", Some(artifact)))
        .await
        .unwrap();
    let Catalogued::Disc { disc, title_found } = outcome else {
        panic!("expected a disc");
    };
    assert!(!title_found);

    let found = find_discs(
        &pool,
        DiscSearch {
            disc: Some(disc),
            artifact: Some(artifact),
            limit: 5,
            ..DiscSearch::default()
        },
    )
    .await
    .unwrap();
    let [place] = found.as_slice() else {
        panic!("one disc: {found:?}");
    };
    assert_eq!(place.title, title);
    assert_eq!(place.title_kind, "movie");
    assert_eq!(place.release_year, Some(1995));
    assert_eq!(place.edition, "DVD");
    assert_eq!(place.disc_set, "Single disc");
    assert_eq!(place.disc_count_expected, Some(1));
    assert_eq!(place.disc.sequence_number, 1);
    assert_eq!(place.disc.media_family, "dvd");
    assert_eq!(place.disc.artifact_count, 1);
    assert!(place.linked);

    let audited: (String, serde_json::Value) = sqlx::query_as(
        "SELECT actor_id, metadata FROM audit_events
         WHERE action = 'disc.catalogued' AND target_id = $1",
    )
    .bind(disc.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited.0, "admin");
    assert_eq!(audited.1["artifact_id"], artifact.to_string());

    // The same title again, in another case: a second edition of it.
    let again = catalogue_disc(&pool, new(&title.to_uppercase(), "Blu-ray", None))
        .await
        .unwrap();
    assert!(matches!(
        again,
        Catalogued::Disc {
            title_found: true,
            ..
        }
    ));
    let titles: i64 =
        sqlx::query_scalar("SELECT count(*) FROM titles WHERE lower(display_title) = lower($1)")
            .bind(&title)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(titles, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_artifact_catalogues_nothing() {
    let pool = pool().await;
    let title = unique("Nothing");
    let outcome = catalogue_disc(&pool, new(&title, "DVD", Some(ArtifactId::generate())))
        .await
        .unwrap();
    assert_eq!(outcome, Catalogued::NoArtifact);
    let titles: i64 = sqlx::query_scalar("SELECT count(*) FROM titles WHERE display_title = $1")
        .bind(&title)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(titles, 0);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn discs_are_found_by_every_word_and_the_linked_come_first() {
    let pool = pool().await;
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let first = format!("Aardvark {tag}");
    let second = format!("Zebra {tag}");
    let artifact = artifact(&pool).await;
    let aardvark = disc_of(
        catalogue_disc(&pool, new(&first, "NTSC DVD", None))
            .await
            .unwrap(),
    );
    let zebra = disc_of(
        catalogue_disc(&pool, new(&second, "PAL DVD", Some(artifact)))
            .await
            .unwrap(),
    );

    let search = |text: &'static str| {
        let pool = pool.clone();
        let tag = tag.clone();
        async move {
            let words = format!("{tag} {text}");
            find_discs(
                &pool,
                DiscSearch {
                    search: Some(&words),
                    artifact: Some(artifact),
                    limit: 10,
                    ..DiscSearch::default()
                },
            )
            .await
            .unwrap()
            .iter()
            .map(|place| place.disc.id)
            .collect::<Vec<_>>()
        }
    };
    // Every word must match something about the disc: its title, edition or
    // year here.
    assert_eq!(search("").await, [zebra, aardvark], "the linked one first");
    assert_eq!(search("ntsc").await, [aardvark]);
    assert_eq!(search("zebra 1995 pal").await, [zebra]);
    assert!(search("zebra ntsc").await.is_empty());
    assert!(search("1996").await.is_empty());
}
