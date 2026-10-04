// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! The catalog: titles, editions, disc sets, and discs.
//!
//! The canonical object of this system is a disc, not a film or a game, and
//! the shape here says so. A title is a work; an edition is a particular
//! release of it; a disc set is what that release shipped as; a disc is one
//! physical thing in that set. Content categories are metadata layered over
//! the same model, never a different model.
//!
//! Four levels is more than a shortcut would need, and the shortcut is the
//! reason for the depth. A film reissued in three regions with different
//! supplements is one title, three editions, and possibly seven discs, and
//! every burn, artifact link and physical copy hangs off the disc rather than
//! off the film. Collapsing the middle would make "which disc is this" a
//! question the data could not answer.
//!
//! These are the rows a burn job points at. Until they existed the only way to
//! queue a burn was to write a disc row by hand.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::repositories::{
    CatalogOutcome, DiscRecord, DiscSetRecord, EditionRecord, NewDisc, NewDiscSet, NewEdition,
    NewTitle, TitleRecord, create_disc, create_disc_set, create_edition, create_title, get_disc,
    get_disc_set, get_edition, get_title, link_artifact_to_disc, list_disc_artifacts,
    list_disc_sets, list_discs, list_editions, list_titles,
};
use tangible_domain::{
    ArtifactId, DiscId, DiscRelationship, DiscSetId, EditionId, MediaFamily, SetKind, TitleId,
    TitleKind,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// Largest page a caller may ask for.
const MAX_LIMIT: usize = 500;

/// Default page size.
const DEFAULT_LIMIT: usize = 50;

/// Longest name accepted, matching the columns.
const MAX_NAME: usize = 500;

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

fn limit_of(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn as_i64(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::try_from(DEFAULT_LIMIT).unwrap_or(50))
}

fn unavailable(detail: &str) -> Problem {
    Problem::new(ErrorCode::StorageUnavailable, detail)
}

// --- views ---------------------------------------------------------------------

/// A work.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TitleView {
    /// Opaque identifier.
    pub id: String,
    /// What kind of work it is.
    pub kind: String,
    /// What it is called.
    pub display_title: String,
    /// What it sorts as.
    pub sort_title: String,
    /// When it was released.
    pub release_year: Option<i16>,
    /// How many editions it has.
    pub edition_count: i64,
    /// When it was catalogued.
    pub created_at: String,
}

impl From<TitleRecord> for TitleView {
    fn from(record: TitleRecord) -> Self {
        Self {
            id: record.id.to_string(),
            kind: record.kind,
            display_title: record.display_title,
            sort_title: record.sort_title,
            release_year: record.release_year,
            edition_count: record.edition_count,
            created_at: rfc3339(record.created_at),
        }
    }
}

/// A page of titles.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TitlePage {
    /// The titles in this page, in sort order.
    pub items: Vec<TitleView>,
    /// The sort key to pass as `after` to continue, or null at the end.
    ///
    /// The sort title rather than an opaque cursor, because the ordering is
    /// alphabetical and a client showing "continue from R" should be able to
    /// say so.
    pub next_after: Option<String>,
}

/// A particular release of a title.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EditionView {
    /// Opaque identifier.
    pub id: String,
    /// The title it belongs to.
    pub title_id: String,
    /// What this release is called.
    pub display_name: String,
    /// Who published it.
    pub publisher: Option<String>,
    /// Which region it was sold in.
    pub region: Option<String>,
    /// How many disc sets it has.
    pub set_count: i64,
    /// When it was catalogued.
    pub created_at: String,
}

impl From<EditionRecord> for EditionView {
    fn from(record: EditionRecord) -> Self {
        Self {
            id: record.id.to_string(),
            title_id: record.title_id.to_string(),
            display_name: record.display_name,
            publisher: record.publisher,
            region: record.region,
            set_count: record.set_count,
            created_at: rfc3339(record.created_at),
        }
    }
}

/// The discs an edition shipped as.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DiscSetView {
    /// Opaque identifier.
    pub id: String,
    /// The edition it belongs to.
    pub edition_id: String,
    /// What the set is called.
    pub name: String,
    /// What shape of set it is.
    pub set_kind: String,
    /// How many discs it should contain.
    pub disc_count_expected: Option<i16>,
    /// How many are catalogued.
    ///
    /// Beside the expected count so an incomplete set is visible without
    /// counting rows by eye.
    pub disc_count: i64,
    /// When it was catalogued.
    pub created_at: String,
}

impl From<DiscSetRecord> for DiscSetView {
    fn from(record: DiscSetRecord) -> Self {
        Self {
            id: record.id.to_string(),
            edition_id: record.edition_id.to_string(),
            name: record.name,
            set_kind: record.set_kind,
            disc_count_expected: record.disc_count_expected,
            disc_count: record.disc_count,
            created_at: rfc3339(record.created_at),
        }
    }
}

/// One disc.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DiscView {
    /// Opaque identifier.
    pub id: String,
    /// The set it belongs to.
    pub disc_set_id: String,
    /// Which disc of the set it is.
    pub sequence_number: i16,
    /// What it is called, when it has a name of its own.
    pub display_name: Option<String>,
    /// What kind of medium it is.
    pub media_family: String,
    /// Which region it plays in.
    pub region: Option<String>,
    /// The volume label read from an image of it.
    pub volume_label: Option<String>,
    /// What reproduction is expected to achieve.
    ///
    /// `unknown` unless evidence has been recorded, which is the schema's own
    /// rule: anything stronger must be backed by something.
    pub compatibility_claim: String,
    /// How many artifacts are linked to it.
    pub artifact_count: i64,
    /// How many physical copies of it this system has made.
    pub copy_count: i64,
    /// When it was catalogued.
    pub created_at: String,
}

impl From<DiscRecord> for DiscView {
    fn from(record: DiscRecord) -> Self {
        Self {
            id: record.id.to_string(),
            disc_set_id: record.disc_set_id.to_string(),
            sequence_number: record.sequence_number,
            display_name: record.display_name,
            media_family: record.media_family,
            region: record.region,
            volume_label: record.volume_label,
            compatibility_claim: record.compatibility_claim,
            artifact_count: record.artifact_count,
            copy_count: record.copy_count,
            created_at: rfc3339(record.created_at),
        }
    }
}

/// An artifact linked to a disc.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DiscArtifactView {
    /// The artifact.
    pub artifact_id: String,
    /// What the artifact is to the disc.
    pub relationship: String,
    /// How confident the link is, from 0 to 1.
    pub confidence: f32,
}

// --- requests ------------------------------------------------------------------

/// A new title.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateTitleRequest {
    /// What it is called.
    pub display_title: String,
    /// What it sorts as. Defaults to the display title.
    #[serde(default)]
    pub sort_title: Option<String>,
    /// What kind of work it is. Defaults to unknown.
    #[serde(default)]
    pub kind: Option<String>,
    /// When it was released.
    #[serde(default)]
    pub release_year: Option<i16>,
}

/// A new edition.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateEditionRequest {
    /// What this release is called.
    pub display_name: String,
    /// Who published it.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Which region it was sold in.
    #[serde(default)]
    pub region: Option<String>,
}

/// A new disc set.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateDiscSetRequest {
    /// What the set is called.
    pub name: String,
    /// What shape of set it is. Defaults to unknown.
    #[serde(default)]
    pub set_kind: Option<String>,
    /// How many discs it should contain.
    #[serde(default)]
    pub disc_count_expected: Option<i16>,
}

/// A new disc.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateDiscRequest {
    /// Which disc of the set it is. Starts at 1.
    pub sequence_number: i16,
    /// What it is called, when it has a name of its own.
    #[serde(default)]
    pub display_name: Option<String>,
    /// What kind of medium it is. Defaults to unknown.
    #[serde(default)]
    pub media_family: Option<String>,
    /// Which region it plays in.
    #[serde(default)]
    pub region: Option<String>,
}

/// A link between an artifact and a disc.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LinkArtifactRequest {
    /// The artifact.
    pub artifact_id: String,
    /// What the artifact is to the disc. Defaults to a representation of it.
    #[serde(default)]
    pub relationship: Option<String>,
    /// How confident the link is, from 0 to 1. Defaults to certain, because
    /// an operator linking by hand is the strongest evidence there is.
    #[serde(default)]
    pub confidence: Option<f32>,
}

/// Query parameters for the title listing.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct TitleQuery {
    /// Maximum titles to return. Clamped.
    pub limit: Option<usize>,
    /// Continue after this sort title.
    pub after: Option<String>,
    /// Only titles whose name contains this.
    pub search: Option<String>,
}

fn bounded(value: &str, name: &str) -> Result<(), Problem> {
    if value.trim().is_empty() || value.len() > MAX_NAME {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!("{name} must be between 1 and {MAX_NAME} characters"),
        ));
    }
    Ok(())
}

/// Turn a refused create into a problem.
fn catalog_problem(outcome: CatalogOutcome, parent: &str) -> Problem {
    match outcome {
        CatalogOutcome::ParentMissing => Problem::new(
            ErrorCode::ReferenceNotFound,
            format!("no {parent} with that identifier"),
        ),
        CatalogOutcome::SequenceTaken => Problem::new(
            ErrorCode::Conflict,
            "that disc number is already in this set; two discs with one number \
             would make the set ambiguous",
        ),
    }
}

// --- titles --------------------------------------------------------------------

/// List titles, in sort order.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a malformed filter.
#[utoipa::path(
    get,
    path = "/api/v1/titles",
    tag = "catalog",
    description = "List titles in sort order. Pass `search` to filter by name \
                   and `next_after` from the response to continue.",
    params(TitleQuery),
    responses((status = 200, description = "A page of titles", body = TitlePage)),
)]
pub async fn list_all_titles(
    State(state): State<ApiState>,
    Query(query): Query<TitleQuery>,
) -> Result<Json<TitlePage>, Problem> {
    let limit = limit_of(query.limit);
    let titles = list_titles(
        state.database().pool(),
        query.search.as_deref(),
        as_i64(limit),
        query.after.as_deref(),
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not list titles");
        unavailable("the catalog could not be read")
    })?;

    let next_after = if titles.len() < limit {
        None
    } else {
        titles.last().map(|title| title.sort_title.clone())
    };

    Ok(Json(TitlePage {
        items: titles.into_iter().map(TitleView::from).collect(),
        next_after,
    }))
}

/// Record a title.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a name this server will not store.
#[utoipa::path(
    post,
    path = "/api/v1/titles",
    tag = "catalog",
    description = "Record a work. Editions, disc sets and discs hang off it.",
    request_body = CreateTitleRequest,
    responses(
        (status = 201, description = "Recorded", body = TitleView),
        (status = 422, description = "The request will not be stored", body = Problem),
    ),
)]
pub async fn create_new_title(
    State(state): State<ApiState>,
    Json(request): Json<CreateTitleRequest>,
) -> Result<axum::response::Response, Problem> {
    bounded(&request.display_title, "display_title")?;

    let kind = match &request.kind {
        None => TitleKind::Unknown,
        Some(raw) => raw.parse::<TitleKind>().map_err(|_| {
            Problem::new(
                ErrorCode::ValidationFailed,
                "that is not a kind of work this server knows",
            )
        })?,
    };
    // Defaulting to the display title rather than deriving one: stripping a
    // leading article is a language-specific decision, and getting it wrong
    // for somebody else's language is worse than sorting plainly.
    let sort_title = request
        .sort_title
        .clone()
        .unwrap_or_else(|| request.display_title.clone());
    bounded(&sort_title, "sort_title")?;

    let title = create_title(
        state.database().pool(),
        NewTitle {
            kind: kind.as_str(),
            display_title: request.display_title.trim(),
            sort_title: sort_title.trim(),
            release_year: request.release_year,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not record a title");
        unavailable("the title could not be recorded")
    })?;

    tracing::info!(title_id = %title.id, "title recorded");
    Ok((StatusCode::CREATED, Json(TitleView::from(title))).into_response())
}

/// One title.
///
/// # Errors
///
/// `NOT_FOUND` if no such title exists.
#[utoipa::path(
    get,
    path = "/api/v1/titles/{title_id}",
    tag = "catalog",
    description = "Fetch one title.",
    params(("title_id" = String, Path, description = "Title identifier")),
    responses(
        (status = 200, description = "The title", body = TitleView),
        (status = 404, description = "No such title", body = Problem),
    ),
)]
pub async fn get_one_title(
    State(state): State<ApiState>,
    Path(title_id): Path<String>,
) -> Result<Json<TitleView>, Problem> {
    let id = title_id
        .parse::<TitleId>()
        .map_err(|_| Problem::invalid_parameter("title_id", "not a valid identifier"))?;
    let title = get_title(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a title");
            unavailable("the title could not be read")
        })?
        .ok_or_else(|| Problem::not_found("title", &id.to_string()))?;
    Ok(Json(TitleView::from(title)))
}

// --- editions ------------------------------------------------------------------

/// The editions of a title.
///
/// # Errors
///
/// `NOT_FOUND` if no such title exists.
#[utoipa::path(
    get,
    path = "/api/v1/titles/{title_id}/editions",
    tag = "catalog",
    description = "List the editions of a title.",
    params(("title_id" = String, Path, description = "Title identifier")),
    responses(
        (status = 200, description = "The editions", body = Vec<EditionView>),
        (status = 404, description = "No such title", body = Problem),
    ),
)]
pub async fn list_title_editions(
    State(state): State<ApiState>,
    Path(title_id): Path<String>,
) -> Result<Json<Vec<EditionView>>, Problem> {
    let id = title_id
        .parse::<TitleId>()
        .map_err(|_| Problem::invalid_parameter("title_id", "not a valid identifier"))?;

    // Confirmed first so an unknown title is a 404 rather than an empty list,
    // which would read as "this title has no editions".
    if get_title(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a title");
            unavailable("the title could not be read")
        })?
        .is_none()
    {
        return Err(Problem::not_found("title", &id.to_string()));
    }

    let editions = list_editions(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not list editions");
            unavailable("the editions could not be read")
        })?;
    Ok(Json(editions.into_iter().map(EditionView::from).collect()))
}

/// Record an edition of a title.
///
/// # Errors
///
/// `REFERENCE_NOT_FOUND` if the title does not exist.
#[utoipa::path(
    post,
    path = "/api/v1/titles/{title_id}/editions",
    tag = "catalog",
    description = "Record a particular release of a title.",
    params(("title_id" = String, Path, description = "Title identifier")),
    request_body = CreateEditionRequest,
    responses(
        (status = 201, description = "Recorded", body = EditionView),
        (status = 422, description = "No such title, or an unusable name", body = Problem),
    ),
)]
pub async fn create_title_edition(
    State(state): State<ApiState>,
    Path(title_id): Path<String>,
    Json(request): Json<CreateEditionRequest>,
) -> Result<axum::response::Response, Problem> {
    let id = title_id
        .parse::<TitleId>()
        .map_err(|_| Problem::invalid_parameter("title_id", "not a valid identifier"))?;
    bounded(&request.display_name, "display_name")?;

    let edition = create_edition(
        state.database().pool(),
        NewEdition {
            title_id: id,
            display_name: request.display_name.trim(),
            publisher: request.publisher.as_deref(),
            region: request.region.as_deref(),
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not record an edition");
        unavailable("the edition could not be recorded")
    })?
    .map_err(|outcome| catalog_problem(outcome, "title"))?;

    tracing::info!(edition_id = %edition.id, "edition recorded");
    Ok((StatusCode::CREATED, Json(EditionView::from(edition))).into_response())
}

/// One edition, with its disc sets.
///
/// # Errors
///
/// `NOT_FOUND` if no such edition exists.
#[utoipa::path(
    get,
    path = "/api/v1/editions/{edition_id}/disc-sets",
    tag = "catalog",
    description = "List the disc sets of an edition.",
    params(("edition_id" = String, Path, description = "Edition identifier")),
    responses(
        (status = 200, description = "The disc sets", body = Vec<DiscSetView>),
        (status = 404, description = "No such edition", body = Problem),
    ),
)]
pub async fn list_edition_sets(
    State(state): State<ApiState>,
    Path(edition_id): Path<String>,
) -> Result<Json<Vec<DiscSetView>>, Problem> {
    let id = edition_id
        .parse::<EditionId>()
        .map_err(|_| Problem::invalid_parameter("edition_id", "not a valid identifier"))?;

    if get_edition(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read an edition");
            unavailable("the edition could not be read")
        })?
        .is_none()
    {
        return Err(Problem::not_found("edition", &id.to_string()));
    }

    let sets = list_disc_sets(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not list disc sets");
            unavailable("the disc sets could not be read")
        })?;
    Ok(Json(sets.into_iter().map(DiscSetView::from).collect()))
}

/// Record a disc set within an edition.
///
/// # Errors
///
/// `REFERENCE_NOT_FOUND` if the edition does not exist.
#[utoipa::path(
    post,
    path = "/api/v1/editions/{edition_id}/disc-sets",
    tag = "catalog",
    description = "Record what an edition shipped as.",
    params(("edition_id" = String, Path, description = "Edition identifier")),
    request_body = CreateDiscSetRequest,
    responses(
        (status = 201, description = "Recorded", body = DiscSetView),
        (status = 422, description = "No such edition, or an unusable name", body = Problem),
    ),
)]
pub async fn create_edition_set(
    State(state): State<ApiState>,
    Path(edition_id): Path<String>,
    Json(request): Json<CreateDiscSetRequest>,
) -> Result<axum::response::Response, Problem> {
    let id = edition_id
        .parse::<EditionId>()
        .map_err(|_| Problem::invalid_parameter("edition_id", "not a valid identifier"))?;
    bounded(&request.name, "name")?;

    let set_kind = match &request.set_kind {
        None => SetKind::Unknown,
        Some(raw) => raw.parse::<SetKind>().map_err(|_| {
            Problem::new(
                ErrorCode::ValidationFailed,
                "that is not a kind of set this server knows",
            )
        })?,
    };

    let set = create_disc_set(
        state.database().pool(),
        NewDiscSet {
            edition_id: id,
            name: request.name.trim(),
            set_kind: set_kind.as_str(),
            disc_count_expected: request.disc_count_expected,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not record a disc set");
        unavailable("the disc set could not be recorded")
    })?
    .map_err(|outcome| catalog_problem(outcome, "edition"))?;

    tracing::info!(disc_set_id = %set.id, "disc set recorded");
    Ok((StatusCode::CREATED, Json(DiscSetView::from(set))).into_response())
}

// --- discs ---------------------------------------------------------------------

/// The discs in a set.
///
/// # Errors
///
/// `NOT_FOUND` if no such set exists.
#[utoipa::path(
    get,
    path = "/api/v1/disc-sets/{disc_set_id}/discs",
    tag = "catalog",
    description = "List the discs in a set, in order.",
    params(("disc_set_id" = String, Path, description = "Disc set identifier")),
    responses(
        (status = 200, description = "The discs", body = Vec<DiscView>),
        (status = 404, description = "No such disc set", body = Problem),
    ),
)]
pub async fn list_set_discs(
    State(state): State<ApiState>,
    Path(disc_set_id): Path<String>,
) -> Result<Json<Vec<DiscView>>, Problem> {
    let id = disc_set_id
        .parse::<DiscSetId>()
        .map_err(|_| Problem::invalid_parameter("disc_set_id", "not a valid identifier"))?;

    if get_disc_set(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a disc set");
            unavailable("the disc set could not be read")
        })?
        .is_none()
    {
        return Err(Problem::not_found("disc set", &id.to_string()));
    }

    let discs = list_discs(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not list discs");
            unavailable("the discs could not be read")
        })?;
    Ok(Json(discs.into_iter().map(DiscView::from).collect()))
}

/// Record a disc within a set.
///
/// # Errors
///
/// `REFERENCE_NOT_FOUND` if the set does not exist, or `CONFLICT` if that disc
/// number is already taken.
#[utoipa::path(
    post,
    path = "/api/v1/disc-sets/{disc_set_id}/discs",
    tag = "catalog",
    description = "Record one disc in a set. Disc numbers are unique within \
                   the set.",
    params(("disc_set_id" = String, Path, description = "Disc set identifier")),
    request_body = CreateDiscRequest,
    responses(
        (status = 201, description = "Recorded", body = DiscView),
        (status = 409, description = "That disc number is taken", body = Problem),
        (status = 422, description = "No such set, or an unusable value", body = Problem),
    ),
)]
pub async fn create_set_disc(
    State(state): State<ApiState>,
    Path(disc_set_id): Path<String>,
    Json(request): Json<CreateDiscRequest>,
) -> Result<axum::response::Response, Problem> {
    let id = disc_set_id
        .parse::<DiscSetId>()
        .map_err(|_| Problem::invalid_parameter("disc_set_id", "not a valid identifier"))?;

    if request.sequence_number < 1 {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "disc numbers start at 1",
        ));
    }
    let media_family = match &request.media_family {
        None => MediaFamily::Unknown,
        Some(raw) => raw.parse::<MediaFamily>().map_err(|_| {
            Problem::new(
                ErrorCode::ValidationFailed,
                "that is not a kind of medium this server knows",
            )
        })?,
    };

    let disc = create_disc(
        state.database().pool(),
        NewDisc {
            disc_set_id: id,
            sequence_number: request.sequence_number,
            display_name: request.display_name.as_deref(),
            media_family: media_family.as_str(),
            region: request.region.as_deref(),
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not record a disc");
        unavailable("the disc could not be recorded")
    })?
    .map_err(|outcome| catalog_problem(outcome, "disc set"))?;

    tracing::info!(disc_id = %disc.id, "disc recorded");
    Ok((StatusCode::CREATED, Json(DiscView::from(disc))).into_response())
}

/// One disc.
///
/// # Errors
///
/// `NOT_FOUND` if no such disc exists.
#[utoipa::path(
    get,
    path = "/api/v1/discs/{disc_id}",
    tag = "catalog",
    description = "Fetch one disc, the canonical object of this system.",
    params(("disc_id" = String, Path, description = "Disc identifier")),
    responses(
        (status = 200, description = "The disc", body = DiscView),
        (status = 404, description = "No such disc", body = Problem),
    ),
)]
pub async fn get_one_disc(
    State(state): State<ApiState>,
    Path(disc_id): Path<String>,
) -> Result<Json<DiscView>, Problem> {
    let id = disc_id
        .parse::<DiscId>()
        .map_err(|_| Problem::invalid_parameter("disc_id", "not a valid identifier"))?;
    let disc = get_disc(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a disc");
            unavailable("the disc could not be read")
        })?
        .ok_or_else(|| Problem::not_found("disc", &id.to_string()))?;
    Ok(Json(DiscView::from(disc)))
}

/// The artifacts linked to a disc.
///
/// # Errors
///
/// `NOT_FOUND` if no such disc exists.
#[utoipa::path(
    get,
    path = "/api/v1/discs/{disc_id}/artifacts",
    tag = "catalog",
    description = "List the artifacts linked to a disc.",
    params(("disc_id" = String, Path, description = "Disc identifier")),
    responses(
        (status = 200, description = "The links", body = Vec<DiscArtifactView>),
        (status = 404, description = "No such disc", body = Problem),
    ),
)]
pub async fn list_linked_artifacts(
    State(state): State<ApiState>,
    Path(disc_id): Path<String>,
) -> Result<Json<Vec<DiscArtifactView>>, Problem> {
    let id = disc_id
        .parse::<DiscId>()
        .map_err(|_| Problem::invalid_parameter("disc_id", "not a valid identifier"))?;

    if get_disc(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a disc");
            unavailable("the disc could not be read")
        })?
        .is_none()
    {
        return Err(Problem::not_found("disc", &id.to_string()));
    }

    let links = list_disc_artifacts(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not list disc artifacts");
            unavailable("the links could not be read")
        })?;

    Ok(Json(
        links
            .into_iter()
            .map(|(artifact_id, relationship, confidence)| DiscArtifactView {
                artifact_id: artifact_id.to_string(),
                relationship,
                confidence,
            })
            .collect(),
    ))
}

/// Link an artifact to the disc it represents.
///
/// Idempotent by the pair: linking the same artifact again updates what is
/// claimed about the relationship rather than failing, because an operator
/// correcting a claim should not have to unlink first.
///
/// # Errors
///
/// `REFERENCE_NOT_FOUND` if the disc or artifact does not exist.
#[utoipa::path(
    post,
    path = "/api/v1/discs/{disc_id}/artifact-links",
    tag = "catalog",
    description = "Link an artifact to a disc, saying what it is to that disc. \
                   Linking again updates the claim rather than failing.",
    params(("disc_id" = String, Path, description = "Disc identifier")),
    request_body = LinkArtifactRequest,
    responses(
        (status = 200, description = "Linked", body = Vec<DiscArtifactView>),
        (status = 422, description = "No such disc or artifact", body = Problem),
    ),
)]
pub async fn link_artifact(
    State(state): State<ApiState>,
    Path(disc_id): Path<String>,
    Json(request): Json<LinkArtifactRequest>,
) -> Result<Json<Vec<DiscArtifactView>>, Problem> {
    let id = disc_id
        .parse::<DiscId>()
        .map_err(|_| Problem::invalid_parameter("disc_id", "not a valid identifier"))?;
    let artifact_id = request
        .artifact_id
        .parse::<ArtifactId>()
        .map_err(|_| Problem::invalid_parameter("artifact_id", "not a valid identifier"))?;

    let relationship = match &request.relationship {
        None => DiscRelationship::RepresentationOf,
        Some(raw) => raw.parse::<DiscRelationship>().map_err(|_| {
            Problem::new(
                ErrorCode::ValidationFailed,
                "that is not a relationship this server knows",
            )
        })?,
    };
    let confidence = request.confidence.unwrap_or(1.0);
    if !(0.0..=1.0).contains(&confidence) {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "confidence must be between 0 and 1",
        ));
    }

    link_artifact_to_disc(
        state.database().pool(),
        artifact_id,
        id,
        relationship.as_str(),
        confidence,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not link an artifact");
        unavailable("the link could not be recorded")
    })?
    .map_err(|outcome| catalog_problem(outcome, "disc or artifact"))?;

    tracing::info!(disc_id = %id, artifact_id = %artifact_id, "artifact linked to disc");
    // A disc gaining an image can complete a game's RomM export.
    if let Some(romm) = state.romm() {
        romm.nudge();
    }
    list_linked_artifacts(State(state), Path(disc_id)).await
}

/// The catalog routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/titles", get(list_all_titles).post(create_new_title))
        .route("/titles/{title_id}", get(get_one_title))
        .route(
            "/titles/{title_id}/editions",
            get(list_title_editions).post(create_title_edition),
        )
        .route(
            "/editions/{edition_id}/disc-sets",
            get(list_edition_sets).post(create_edition_set),
        )
        .route(
            "/disc-sets/{disc_set_id}/discs",
            get(list_set_discs).post(create_set_disc),
        )
        .route("/discs/{disc_id}", get(get_one_disc))
        .route("/discs/{disc_id}/artifacts", get(list_linked_artifacts))
        .route("/discs/{disc_id}/artifact-links", post(link_artifact))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_name_must_be_something_the_column_will_hold() {
        assert!(bounded("Example Disc", "display_title").is_ok());
        assert!(bounded("   ", "display_title").is_err());
        assert!(bounded("", "display_title").is_err());
        assert!(bounded(&"x".repeat(MAX_NAME + 1), "display_title").is_err());
    }

    #[test]
    fn a_repeated_disc_number_is_a_conflict_with_a_reason() {
        // Two discs numbered two would make the set ambiguous forever, so the
        // refusal says why rather than reporting a constraint.
        let problem = catalog_problem(CatalogOutcome::SequenceTaken, "disc set");
        assert_eq!(problem.code, "CONFLICT");
        assert!(problem.detail.contains("ambiguous"), "{}", problem.detail);
    }

    #[test]
    fn a_missing_parent_names_what_was_missing() {
        let problem = catalog_problem(CatalogOutcome::ParentMissing, "edition");
        assert_eq!(problem.code, "REFERENCE_NOT_FOUND");
        assert!(problem.detail.contains("edition"));
    }

    #[test]
    fn a_limit_is_clamped_rather_than_rejected() {
        assert_eq!(limit_of(None), DEFAULT_LIMIT);
        assert_eq!(limit_of(Some(0)), 1);
        assert_eq!(limit_of(Some(usize::MAX)), MAX_LIMIT);
    }
}
