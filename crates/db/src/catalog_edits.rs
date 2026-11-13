// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Editing the catalog (docs/plans/catalog-editing.md, API.md 11).
//!
//! Each edit locks its row, checks the version it was made against when the
//! caller names one, changes only the fields that differ, adds one to the
//! version and audits which fields changed, all in one transaction. An edit
//! that changes nothing changes nothing: no new version and no audit row.
//! Creating rows is in [`crate::repositories`].

use sqlx::{PgPool, Postgres, Row as _, Transaction};
use tangible_domain::{DiscId, DiscSetId, EditionId, TitleId};

use crate::DbError;
use crate::repositories::{
    DISC_SELECT, DISC_SET_SELECT, DiscRecord, DiscSetRecord, EDITION_SELECT, EditionRecord,
    TITLE_SELECT, TitleRecord, disc_from_row, disc_set_from_row, edition_from_row, title_from_row,
};

/// A change to a field that may be empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Change<T> {
    /// Leave it as it is.
    #[default]
    Keep,
    /// Empty it.
    Clear,
    /// Set it to this.
    Set(T),
}

impl<T> Change<T> {
    /// The field's value after the change, or `None` when it is left.
    #[allow(
        clippy::option_option,
        reason = "merged takes an edit that may be absent"
    )]
    fn apply(self) -> Option<Option<T>> {
        match self {
            Self::Keep => None,
            Self::Clear => Some(None),
            Self::Set(value) => Some(Some(value)),
        }
    }
}

/// A text field's change, owned, as [`merged`] takes it.
#[allow(
    clippy::option_option,
    reason = "merged takes an edit that may be absent"
)]
fn owned(change: Change<&str>) -> Option<Option<String>> {
    change.apply().map(|value| value.map(str::to_owned))
}

/// What an edit came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome<T> {
    /// Done, or nothing to do: the row as it now is.
    Done(T),
    /// No such row.
    NotFound,
    /// The edit named a version the row is no longer at; it is at this one.
    Stale(i64),
    /// A disc with that number is already in the set.
    SequenceTaken,
}

/// A title's edit.
#[derive(Debug, Clone, Copy, Default)]
pub struct TitleEdit<'a> {
    /// What kind of work it is.
    pub kind: Option<&'a str>,
    /// What it is called.
    pub display_title: Option<&'a str>,
    /// What it sorts as. Left alone while the display title changes, it
    /// follows the display title if it was the display title before.
    pub sort_title: Option<&'a str>,
    /// When it was released.
    pub release_year: Change<i16>,
}

/// An edition's edit.
#[derive(Debug, Clone, Copy, Default)]
pub struct EditionEdit<'a> {
    /// What this release is called.
    pub display_name: Option<&'a str>,
    /// Who published it.
    pub publisher: Change<&'a str>,
    /// Which region it was sold in.
    pub region: Change<&'a str>,
}

/// A disc set's edit.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscSetEdit<'a> {
    /// What the set is called.
    pub name: Option<&'a str>,
    /// What shape of set it is.
    pub set_kind: Option<&'a str>,
    /// How many discs it should contain.
    pub disc_count_expected: Change<i16>,
}

/// A disc's edit.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscEdit<'a> {
    /// Which disc of the set it is.
    pub sequence_number: Option<i16>,
    /// What it is called, when it has a name of its own.
    pub display_name: Change<&'a str>,
    /// What kind of medium it is.
    pub media_family: Option<&'a str>,
    /// Which region it plays in.
    pub region: Change<&'a str>,
}

/// The value an edit leaves a field at, and whether that is a change.
fn merged<T: PartialEq + Clone>(
    current: &T,
    edit: Option<T>,
    changed: &mut Vec<&'static str>,
    name: &'static str,
) -> T {
    match edit {
        Some(value) if &value != current => {
            changed.push(name);
            value
        }
        _ => current.clone(),
    }
}

/// Lock a row and check the version an edit names, answering the row's
/// current fields, or why not.
async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    select: &'static str,
    id: uuid::Uuid,
    expected: Option<i64>,
) -> Result<Result<sqlx::postgres::PgRow, EditFailure>, DbError> {
    let Some(row) = sqlx::query(select)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(DbError::Query)?
    else {
        return Ok(Err(EditFailure::NotFound));
    };
    let version: i64 = row.try_get("version").map_err(DbError::Query)?;
    if expected.is_some_and(|expected| expected != version) {
        return Ok(Err(EditFailure::Stale(version)));
    }
    Ok(Ok(row))
}

/// Why an edit stopped before changing anything.
enum EditFailure {
    NotFound,
    Stale(i64),
}

impl EditFailure {
    const fn outcome<T>(self) -> EditOutcome<T> {
        match self {
            Self::NotFound => EditOutcome::NotFound,
            Self::Stale(version) => EditOutcome::Stale(version),
        }
    }
}

/// Audit an edit as `action` by `actor`, naming the fields it changed and
/// the version it made.
async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    actor: &str,
    action: &str,
    id: uuid::Uuid,
    changed: &[&str],
) -> Result<(), DbError> {
    let version: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT version FROM {} WHERE id = $1",
        match action {
            "title.updated" => "titles",
            "edition.updated" => "editions",
            "disc_set.updated" => "disc_sets",
            _ => "discs",
        }
    )))
    .bind(id)
    .fetch_one(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    crate::accounts::audit(
        tx,
        actor,
        action,
        &id.to_string(),
        serde_json::json!({ "fields": changed, "version": version }),
    )
    .await
}

/// Edit a title.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn edit_title(
    pool: &PgPool,
    id: TitleId,
    edit: TitleEdit<'_>,
    expected: Option<i64>,
    actor: &str,
) -> Result<EditOutcome<TitleRecord>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let row = match lock(
        &mut tx,
        "SELECT kind, display_title, sort_title, release_year, version FROM titles
         WHERE id = $1 FOR UPDATE",
        *id.as_uuid(),
        expected,
    )
    .await?
    {
        Ok(row) => row,
        Err(failure) => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(failure.outcome());
        }
    };
    let current_kind: String = row.try_get("kind").map_err(DbError::Query)?;
    let current_display: String = row.try_get("display_title").map_err(DbError::Query)?;
    let current_sort: String = row.try_get("sort_title").map_err(DbError::Query)?;
    let current_year: Option<i16> = row.try_get("release_year").map_err(DbError::Query)?;

    let mut changed = Vec::new();
    let kind = merged(
        &current_kind,
        edit.kind.map(str::to_owned),
        &mut changed,
        "kind",
    );
    let display = merged(
        &current_display,
        edit.display_title.map(str::to_owned),
        &mut changed,
        "display_title",
    );
    // A sort title that was only ever the display title follows it.
    let sort_edit = edit.sort_title.map(str::to_owned).or_else(|| {
        (display != current_display && current_sort == current_display).then(|| display.clone())
    });
    let sort = merged(&current_sort, sort_edit, &mut changed, "sort_title");
    let year = merged(
        &current_year,
        edit.release_year.apply(),
        &mut changed,
        "release_year",
    );

    if !changed.is_empty() {
        sqlx::query(
            "UPDATE titles SET kind = $2, display_title = $3, sort_title = $4, release_year = $5,
                 version = version + 1, updated_at = now()
             WHERE id = $1",
        )
        .bind(id.as_uuid())
        .bind(&kind)
        .bind(&display)
        .bind(&sort)
        .bind(year)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
        audit(&mut tx, actor, "title.updated", *id.as_uuid(), &changed).await?;
    }
    // Read in the same transaction, so the version answered is the one
    // this edit made, not a later edit's.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{TITLE_SELECT} WHERE t.id = $1"
    )))
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let record = row.as_ref().map(title_from_row).transpose()?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(record.map_or(EditOutcome::NotFound, EditOutcome::Done))
}

/// Edit an edition.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn edit_edition(
    pool: &PgPool,
    id: EditionId,
    edit: EditionEdit<'_>,
    expected: Option<i64>,
    actor: &str,
) -> Result<EditOutcome<EditionRecord>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let row = match lock(
        &mut tx,
        "SELECT display_name, publisher, region, version FROM editions WHERE id = $1 FOR UPDATE",
        *id.as_uuid(),
        expected,
    )
    .await?
    {
        Ok(row) => row,
        Err(failure) => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(failure.outcome());
        }
    };
    let current_name: String = row.try_get("display_name").map_err(DbError::Query)?;
    let current_publisher: Option<String> = row.try_get("publisher").map_err(DbError::Query)?;
    let current_region: Option<String> = row.try_get("region").map_err(DbError::Query)?;

    let mut changed = Vec::new();
    let name = merged(
        &current_name,
        edit.display_name.map(str::to_owned),
        &mut changed,
        "display_name",
    );
    let publisher = merged(
        &current_publisher,
        owned(edit.publisher),
        &mut changed,
        "publisher",
    );
    let region = merged(&current_region, owned(edit.region), &mut changed, "region");

    if !changed.is_empty() {
        sqlx::query(
            "UPDATE editions SET display_name = $2, publisher = $3, region = $4,
                 version = version + 1, updated_at = now()
             WHERE id = $1",
        )
        .bind(id.as_uuid())
        .bind(&name)
        .bind(&publisher)
        .bind(&region)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
        audit(&mut tx, actor, "edition.updated", *id.as_uuid(), &changed).await?;
    }
    // Read in the same transaction, so the version answered is the one
    // this edit made, not a later edit's.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{EDITION_SELECT} WHERE e.id = $1"
    )))
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let record = row.as_ref().map(edition_from_row).transpose()?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(record.map_or(EditOutcome::NotFound, EditOutcome::Done))
}

/// Edit a disc set.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn edit_disc_set(
    pool: &PgPool,
    id: DiscSetId,
    edit: DiscSetEdit<'_>,
    expected: Option<i64>,
    actor: &str,
) -> Result<EditOutcome<DiscSetRecord>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let row = match lock(
        &mut tx,
        "SELECT name, set_kind, disc_count_expected, version FROM disc_sets
         WHERE id = $1 FOR UPDATE",
        *id.as_uuid(),
        expected,
    )
    .await?
    {
        Ok(row) => row,
        Err(failure) => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(failure.outcome());
        }
    };
    let current_name: String = row.try_get("name").map_err(DbError::Query)?;
    let current_kind: String = row.try_get("set_kind").map_err(DbError::Query)?;
    let current_count: Option<i16> = row.try_get("disc_count_expected").map_err(DbError::Query)?;

    let mut changed = Vec::new();
    let name = merged(
        &current_name,
        edit.name.map(str::to_owned),
        &mut changed,
        "name",
    );
    let kind = merged(
        &current_kind,
        edit.set_kind.map(str::to_owned),
        &mut changed,
        "set_kind",
    );
    let count = merged(
        &current_count,
        edit.disc_count_expected.apply(),
        &mut changed,
        "disc_count_expected",
    );

    if !changed.is_empty() {
        sqlx::query(
            "UPDATE disc_sets SET name = $2, set_kind = $3, disc_count_expected = $4,
                 version = version + 1, updated_at = now()
             WHERE id = $1",
        )
        .bind(id.as_uuid())
        .bind(&name)
        .bind(&kind)
        .bind(count)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
        audit(&mut tx, actor, "disc_set.updated", *id.as_uuid(), &changed).await?;
    }
    // Read in the same transaction, so the version answered is the one
    // this edit made, not a later edit's.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{DISC_SET_SELECT} WHERE s.id = $1"
    )))
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let record = row.as_ref().map(disc_set_from_row).transpose()?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(record.map_or(EditOutcome::NotFound, EditOutcome::Done))
}

/// Edit a disc.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn edit_disc(
    pool: &PgPool,
    id: DiscId,
    edit: DiscEdit<'_>,
    expected: Option<i64>,
    actor: &str,
) -> Result<EditOutcome<DiscRecord>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let row = match lock(
        &mut tx,
        "SELECT sequence_number, display_name, media_family, region, version FROM discs
         WHERE id = $1 FOR UPDATE",
        *id.as_uuid(),
        expected,
    )
    .await?
    {
        Ok(row) => row,
        Err(failure) => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(failure.outcome());
        }
    };
    let current_number: i16 = row.try_get("sequence_number").map_err(DbError::Query)?;
    let current_name: Option<String> = row.try_get("display_name").map_err(DbError::Query)?;
    let current_family: String = row.try_get("media_family").map_err(DbError::Query)?;
    let current_region: Option<String> = row.try_get("region").map_err(DbError::Query)?;

    let mut changed = Vec::new();
    let number = merged(
        &current_number,
        edit.sequence_number,
        &mut changed,
        "sequence_number",
    );
    let name = merged(
        &current_name,
        owned(edit.display_name),
        &mut changed,
        "display_name",
    );
    let family = merged(
        &current_family,
        edit.media_family.map(str::to_owned),
        &mut changed,
        "media_family",
    );
    let region = merged(&current_region, owned(edit.region), &mut changed, "region");

    if !changed.is_empty() {
        let updated = sqlx::query(
            "UPDATE discs SET sequence_number = $2, display_name = $3, media_family = $4,
                 region = $5, version = version + 1, updated_at = now()
             WHERE id = $1",
        )
        .bind(id.as_uuid())
        .bind(number)
        .bind(&name)
        .bind(&family)
        .bind(&region)
        .execute(&mut *tx)
        .await;
        if let Err(error) = updated {
            tx.rollback().await.map_err(DbError::Query)?;
            // Disc numbers are unique within a set, as when it was made.
            if crate::repositories::is_unique_violation(&error) {
                return Ok(EditOutcome::SequenceTaken);
            }
            return Err(DbError::Query(error));
        }
        audit(&mut tx, actor, "disc.updated", *id.as_uuid(), &changed).await?;
    }
    // Read in the same transaction, so the version answered is the one
    // this edit made, not a later edit's.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{DISC_SELECT} WHERE d.id = $1"
    )))
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let record = row.as_ref().map(disc_from_row).transpose()?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(record.map_or(EditOutcome::NotFound, EditOutcome::Done))
}
