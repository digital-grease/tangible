// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Discs found and catalogued where a burn is queued.
//!
//! A burn job is a copy of a catalog disc, and choosing one should not mean
//! walking titles, editions and sets by hand, or remembering an identifier.
//! [`find_discs`] searches every disc by where it sits: its title, edition,
//! set, name, label and year, each word of the search matching one of
//! them. [`catalogue_disc`] records a single-disc release in one step, and
//! links the artifact it is for.

use sqlx::{PgPool, Row as _};
use tangible_domain::{ArtifactId, DiscId, EditionId, TitleId};

use crate::DbError;
use crate::repositories::{DiscRecord, disc_from_row};

/// Most words a search is split into; the rest are not matched.
pub const MAX_WORDS: usize = 8;

/// A disc and where it sits in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscPlace {
    /// The disc.
    pub disc: DiscRecord,
    /// Its title.
    pub title_id: TitleId,
    /// What its title is called.
    pub title: String,
    /// What kind of work its title is.
    pub title_kind: String,
    /// When its title was released.
    pub release_year: Option<i16>,
    /// Its edition.
    pub edition_id: EditionId,
    /// What its edition is called.
    pub edition: String,
    /// What its set is called.
    pub disc_set: String,
    /// How many discs its set should contain.
    pub disc_count_expected: Option<i16>,
    /// Whether the artifact asked about is linked to it.
    pub linked: bool,
}

/// What to look for.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscSearch<'a> {
    /// Words, each of which must be in the disc's title, edition, set, name,
    /// volume label or year. None or blank for every disc.
    pub search: Option<&'a str>,
    /// The artifact a burn is for: discs linked to it come first and say so.
    pub artifact: Option<ArtifactId>,
    /// Only this disc.
    pub disc: Option<DiscId>,
    /// Most discs to answer.
    pub limit: i64,
}

/// The words of a search, at most [`MAX_WORDS`], each as a pattern
/// matching it anywhere and literally: `%` and `_` are not wildcards.
fn words(search: Option<&str>) -> Vec<String> {
    search
        .unwrap_or_default()
        .split_whitespace()
        .take(MAX_WORDS)
        .map(crate::library::contains_pattern)
        .collect()
}

/// Discs matching `search`, those linked to its artifact first, then by
/// title, edition, set and number.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn find_discs(pool: &PgPool, search: DiscSearch<'_>) -> Result<Vec<DiscPlace>, DbError> {
    let rows = sqlx::query(
        "SELECT d.id, d.disc_set_id, d.sequence_number, d.display_name, d.media_family,
                d.region, d.volume_label, d.compatibility_claim, d.created_at, d.version,
                (SELECT count(*) FROM artifact_disc_links l WHERE l.disc_id = d.id)
                    AS artifact_count,
                (SELECT count(*) FROM physical_copies c WHERE c.disc_id = d.id) AS copy_count,
                t.id AS title_id, t.display_title, t.kind AS title_kind, t.release_year,
                e.id AS edition_id, e.display_name AS edition_name,
                s.name AS set_name, s.disc_count_expected,
                ($2::uuid IS NOT NULL AND EXISTS (
                    SELECT 1 FROM artifact_disc_links l
                    WHERE l.disc_id = d.id AND l.artifact_id = $2
                )) AS linked
         FROM discs d
         JOIN disc_sets s ON s.id = d.disc_set_id
         JOIN editions e ON e.id = s.edition_id
         JOIN titles t ON t.id = e.title_id
         WHERE ($3::uuid IS NULL OR d.id = $3)
           AND NOT EXISTS (
               SELECT 1 FROM unnest($1::text[]) AS word
               WHERE concat_ws(' ', t.display_title, t.release_year::text, e.display_name,
                               s.name, d.display_name, d.volume_label)
                     NOT ILIKE word ESCAPE '\\'
           )
         ORDER BY linked DESC, t.sort_title, t.id, e.display_name, e.id, s.name, s.id,
                  d.sequence_number
         LIMIT $4",
    )
    .bind(words(search.search))
    .bind(search.artifact.map(|id| *id.as_uuid()))
    .bind(search.disc.map(|id| *id.as_uuid()))
    .bind(search.limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(place_from_row).collect()
}

fn place_from_row(row: &sqlx::postgres::PgRow) -> Result<DiscPlace, DbError> {
    Ok(DiscPlace {
        disc: disc_from_row(row)?,
        title_id: TitleId::from_uuid(row.try_get("title_id").map_err(DbError::Query)?),
        title: row.try_get("display_title").map_err(DbError::Query)?,
        title_kind: row.try_get("title_kind").map_err(DbError::Query)?,
        release_year: row.try_get("release_year").map_err(DbError::Query)?,
        edition_id: EditionId::from_uuid(row.try_get("edition_id").map_err(DbError::Query)?),
        edition: row.try_get("edition_name").map_err(DbError::Query)?,
        disc_set: row.try_get("set_name").map_err(DbError::Query)?,
        disc_count_expected: row.try_get("disc_count_expected").map_err(DbError::Query)?,
        linked: row.try_get("linked").map_err(DbError::Query)?,
    })
}

/// A single-disc release to catalogue.
#[derive(Debug, Clone, Copy)]
pub struct NewCataloguedDisc<'a> {
    /// What its title is called.
    pub title: &'a str,
    /// What kind of work it is, as `titles.kind` has it.
    pub kind: &'a str,
    /// When it was released.
    pub release_year: Option<i16>,
    /// What this release is called.
    pub edition: &'a str,
    /// Its medium, as `discs.media_family` has it.
    pub media_family: &'a str,
    /// The artifact to link to it, as a representation of it.
    pub artifact: Option<ArtifactId>,
    /// Who catalogued it.
    pub actor: &'a str,
}

/// What cataloguing did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalogued {
    /// The disc, and whether its title was one already catalogued.
    Disc {
        /// The new disc.
        disc: DiscId,
        /// Whether the title was found rather than made.
        title_found: bool,
    },
    /// No such artifact; nothing was catalogued.
    NoArtifact,
}

/// Catalogue a single-disc release: a new edition of the title, or of the
/// title already catalogued under the same name (in any case), kind and year;
/// a set of one disc; disc 1; and the artifact's link to it. All or nothing,
/// and audited.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn catalogue_disc(
    pool: &PgPool,
    new: NewCataloguedDisc<'_>,
) -> Result<Catalogued, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    if let Some(artifact) = new.artifact {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM artifacts WHERE id = $1)")
                .bind(artifact.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(DbError::Query)?;
        if !exists {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(Catalogued::NoArtifact);
        }
    }

    let (title, title_found) = title_for(&mut tx, &new).await?;

    let edition = *EditionId::generate().as_uuid();
    sqlx::query("INSERT INTO editions (id, title_id, display_name) VALUES ($1, $2, $3)")
        .bind(edition)
        .bind(title)
        .bind(new.edition)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    let set = *tangible_domain::DiscSetId::generate().as_uuid();
    sqlx::query(
        "INSERT INTO disc_sets (id, edition_id, name, set_kind, disc_count_expected)
         VALUES ($1, $2, 'Single disc', 'single_disc', 1)",
    )
    .bind(set)
    .bind(edition)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let disc = DiscId::generate();
    sqlx::query(
        "INSERT INTO discs (id, disc_set_id, sequence_number, media_family)
         VALUES ($1, $2, 1, $3)",
    )
    .bind(disc.as_uuid())
    .bind(set)
    .bind(new.media_family)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    if let Some(artifact) = new.artifact {
        sqlx::query(
            "INSERT INTO artifact_disc_links (artifact_id, disc_id, relationship, confidence)
             VALUES ($1, $2, 'representation_of', 1.0)",
        )
        .bind(artifact.as_uuid())
        .bind(disc.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }

    crate::accounts::audit(
        &mut tx,
        new.actor,
        "disc.catalogued",
        &disc.to_string(),
        serde_json::json!({
            "title": new.title,
            "title_id": title.to_string(),
            "title_found": title_found,
            "edition": new.edition,
            "media_family": new.media_family,
            "artifact_id": new.artifact.map(|id| id.to_string()),
        }),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Catalogued::Disc { disc, title_found })
}

/// The title a release is catalogued under: the first already catalogued
/// under its name (in any case), kind and year, or a new one. Says which.
async fn title_for(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    new: &NewCataloguedDisc<'_>,
) -> Result<(uuid::Uuid, bool), DbError> {
    let found: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM titles
         WHERE lower(display_title) = lower($1) AND kind = $2
           AND release_year IS NOT DISTINCT FROM $3
         ORDER BY created_at, id
         LIMIT 1",
    )
    .bind(new.title)
    .bind(new.kind)
    .bind(new.release_year)
    .fetch_optional(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    if let Some(id) = found {
        return Ok((id, true));
    }
    let id = *TitleId::generate().as_uuid();
    sqlx::query(
        "INSERT INTO titles (id, kind, display_title, sort_title, release_year)
         VALUES ($1, $2, $3, $3, $4)",
    )
    .bind(id)
    .bind(new.kind)
    .bind(new.title)
    .bind(new.release_year)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    Ok((id, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_is_its_words_and_no_more_than_eight() {
        assert!(words(None).is_empty());
        assert!(words(Some("  ")).is_empty());
        assert_eq!(words(Some(" Hackers  DVD ")), ["%Hackers%", "%DVD%"]);
        assert_eq!(words(Some("a b c d e f g h i j")).len(), MAX_WORDS);
    }

    #[test]
    fn a_word_matches_itself_and_not_as_a_wildcard() {
        // `%` alone would match every disc, `_` any one character.
        assert_eq!(
            words(Some("100% a_b c\\d")),
            ["%100\\%%", "%a\\_b%", "%c\\\\d%"]
        );
    }
}
