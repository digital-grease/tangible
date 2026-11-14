// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Searching loaded DATs' games by title, as a request is found
//! (docs/plans/requests.md Q2), against a real PostgreSQL.
//!
//! The database is shared, so the DAT here has a name of its own and its
//! games a word nothing else holds; the test removes it after.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::PgPool;
use tangible_db::dats::{NewDat, NewGame, delete_dat, load_dat, search_games};
use tangible_db::{Database, DbConfig};

async fn pool() -> PgPool {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    database.pool().clone()
}

fn game(name: &str) -> NewGame<'_> {
    NewGame {
        name,
        description: None,
        category: None,
        roms: Vec::new(),
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn every_word_is_matched_and_a_wildcard_only_as_itself() {
    let pool = pool().await;
    let tag = format!("q{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
    let dat_name = format!("Test - Console {tag}");
    let names = [
        format!("Final Fantasy VII {tag} (USA) (Disc 1)"),
        format!("Final Fantasy VII {tag} (Europe) (Disc 1)"),
        format!("Final Fantasy Tactics {tag} (USA)"),
        format!("100% Orange {tag} (Japan)"),
        format!("Snake_Eater {tag} (USA)"),
    ];
    let described = format!("Gran Turismo {tag} (USA)");
    let mut games: Vec<NewGame<'_>> = names.iter().map(|name| game(name)).collect();
    games.push(NewGame {
        description: Some("Real Driving Simulator"),
        ..game(&described)
    });
    let loaded = load_dat(
        &pool,
        &NewDat {
            name: &dat_name,
            description: None,
            version: None,
            author: None,
            homepage: None,
            url: None,
            source_sha256: &"ab".repeat(32),
            loaded_by: "test",
            games,
        },
    )
    .await
    .unwrap();

    let search = |query: String| {
        let pool = pool.clone();
        async move { search_games(&pool, &query, 50).await.unwrap() }
    };

    // Every word, in any case, in any order; shortest names first.
    let hits = search(format!("fantasy {tag} final")).await;
    let hit_names: Vec<&str> = hits.iter().map(|hit| hit.name.as_str()).collect();
    assert_eq!(hit_names.len(), 3);
    assert_eq!(hit_names[0], names[2], "the shortest name first");
    assert!(hits.iter().all(|hit| hit.dat_name == dat_name));
    let hits = search(format!("FANTASY vii {tag} usa")).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, names[0]);

    // The description is searched too.
    let hits = search(format!("driving {tag}")).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].description.as_deref(),
        Some("Real Driving Simulator")
    );

    // A % or _ in the query is that character, not a wildcard.
    let hits = search(format!("% {tag}")).await;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].name, names[3]);
    let hits = search(format!("_ {tag}")).await;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].name, names[4]);
    assert!(search(format!("snake%eater {tag}")).await.is_empty());

    // Nothing for nothing, and the limit is kept.
    assert!(search("   ".to_owned()).await.is_empty());
    assert_eq!(search_games(&pool, &tag, 2).await.unwrap().len(), 2);

    assert!(delete_dat(&pool, loaded.id, "test").await.unwrap());
}
