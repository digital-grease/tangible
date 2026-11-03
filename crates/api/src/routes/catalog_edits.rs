// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Editing the catalog (docs/plans/catalog-editing.md, API.md 11).
//!
//! `PATCH` changes the fields given and leaves the rest; `null` clears a
//! field that may be empty. Every catalog row has a version, sent as its
//! `ETag`; an edit sent with `If-Match` naming a version the row has moved
//! past is refused with `409 RESOURCE_VERSION_CONFLICT` rather than
//! overwriting somebody else's change. Without `If-Match` the edit applies.
//! Edits need `catalog`, as creating does, and are audited.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Deserializer};
use tangible_db::catalog_edits::{
    Change, DiscEdit, DiscSetEdit, EditOutcome, EditionEdit, TitleEdit, edit_disc, edit_disc_set,
    edit_edition, edit_title,
};
use tangible_db::repositories::{get_disc_set, get_edition};
use tangible_domain::{DiscId, DiscSetId, EditionId, MediaFamily, SetKind, TitleId, TitleKind};
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::problem::{ErrorCode, Problem};
use crate::routes::catalog::{DiscSetView, DiscView, EditionView, TitleView};
use crate::state::ApiState;

/// Longest name, and longest publisher and region, the columns hold.
const MAX_NAME: usize = 500;
const MAX_PUBLISHER: usize = 300;
const MAX_REGION: usize = 100;

/// Release years the catalog accepts, as its column does.
const RELEASE_YEARS: std::ops::RangeInclusive<i16> = 1870..=2200;

/// A request field that may be empty: left out, it is kept; `null`
/// clears it; a value sets it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Field<T> {
    /// Left out.
    #[default]
    Keep,
    /// `null`.
    Clear,
    /// A value.
    Set(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Only called for a field that is present; one left out is Keep, by
        // `#[serde(default)]`.
        Ok(Option::<T>::deserialize(deserializer)?.map_or(Self::Clear, Self::Set))
    }
}

/// A field that cannot be empty: left out, it is kept; `null` is refused
/// rather than quietly ignored.
fn not_null<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    // Only called for a field that is present; one left out is None, by
    // `#[serde(default)]`.
    T::deserialize(deserializer).map(Some)
}

impl Field<String> {
    /// The change, borrowed.
    fn change(&self) -> Change<&str> {
        match self {
            Self::Keep => Change::Keep,
            Self::Clear => Change::Clear,
            Self::Set(value) => Change::Set(value.as_str()),
        }
    }
}

impl<T: Copy> Field<T> {
    /// The change.
    const fn copied(&self) -> Change<T> {
        match self {
            Self::Keep => Change::Keep,
            Self::Clear => Change::Clear,
            Self::Set(value) => Change::Set(*value),
        }
    }
}

/// A response carrying a catalog row and its version as its `ETag`.
pub fn with_etag<T: serde::Serialize>(view: T, version: i64) -> Response {
    let mut response = Json(view).into_response();
    if let Ok(value) = HeaderValue::from_str(&format!("\"{version}\"")) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// The version an `If-Match` header names: `"7"`, or `W/"7"`; none for no
/// header or `*`.
///
/// # Errors
///
/// `INVALID_PARAMETER` for one that is neither.
fn if_match(headers: &HeaderMap) -> Result<Option<i64>, Problem> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let refused = || {
        Problem::invalid_parameter(
            "If-Match",
            "must be the ETag the resource was read with, such as \"3\"",
        )
    };
    let text = value.to_str().map_err(|_| refused())?.trim();
    if text == "*" {
        return Ok(None);
    }
    let text = text.strip_prefix("W/").unwrap_or(text);
    text.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .and_then(|digits| digits.parse::<i64>().ok())
        .filter(|version| *version > 0)
        .map(Some)
        .ok_or_else(refused)
}

/// A name, trimmed, of 1 to `max` characters.
fn name(value: &str, field: &str, max: usize) -> Result<String, Problem> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > max || trimmed.chars().any(char::is_control)
    {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!(
                "{field} must be between 1 and {max} characters, none of them control \
                 characters; null clears it where it may be empty"
            ),
        ));
    }
    Ok(trimmed.to_owned())
}

/// A text field that may be empty, trimmed and bounded when set.
fn optional(field: &Field<String>, name_of: &str, max: usize) -> Result<Field<String>, Problem> {
    match field {
        Field::Set(value) => name(value, name_of, max).map(Field::Set),
        other => Ok(other.clone()),
    }
}

fn refused_value(what: &str) -> Problem {
    Problem::new(
        ErrorCode::ValidationFailed,
        format!("that is not {what} this server knows"),
    )
}

fn unavailable(detail: &str) -> Problem {
    Problem::new(ErrorCode::StorageUnavailable, detail)
}

/// The answer to an edit: the row with its new version, or why not.
fn answer<R, V: serde::Serialize>(
    outcome: EditOutcome<R>,
    what: &str,
    id: &str,
    version_of: impl Fn(&R) -> i64,
    view: impl Fn(R) -> V,
) -> Result<Response, Problem> {
    match outcome {
        EditOutcome::Done(record) => {
            let version = version_of(&record);
            Ok(with_etag(view(record), version))
        }
        EditOutcome::NotFound => Err(Problem::not_found(what, id)),
        EditOutcome::Stale(version) => Err(Problem::new(
            ErrorCode::ResourceVersionConflict,
            format!(
                "this {what} has changed since it was read; it is at version {version} now, so \
                 read it again before changing it"
            ),
        )),
        EditOutcome::SequenceTaken => Err(Problem::new(
            ErrorCode::Conflict,
            "that disc number is already in this set; two discs with one number would make \
             the set ambiguous",
        )),
    }
}

// --- titles ------------------------------------------------------------------

/// A change to a title. Fields left out are left alone.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditTitleRequest {
    /// What kind of work it is.
    #[serde(default, deserialize_with = "not_null")]
    pub kind: Option<String>,
    /// What it is called.
    #[serde(default, deserialize_with = "not_null")]
    pub display_title: Option<String>,
    /// What it sorts as. Left out while the display title changes, it
    /// follows the display title if it was the display title before.
    #[serde(default, deserialize_with = "not_null")]
    pub sort_title: Option<String>,
    /// When it was released, from 1870 to 2200; null clears it.
    #[serde(default)]
    #[schema(value_type = Option<i16>)]
    pub release_year: Field<i16>,
}

/// Change a title.
///
/// # Errors
///
/// `NOT_FOUND`, `VALIDATION_FAILED`, `RESOURCE_VERSION_CONFLICT`.
#[utoipa::path(
    patch,
    path = "/api/v1/titles/{title_id}",
    tag = "catalog",
    description = "Change the fields given of a title. Send the `ETag` it was read with as \
                   `If-Match` to be refused, rather than overwrite, if it has changed since. \
                   Audited.",
    params(
        ("title_id" = String, Path, description = "Title identifier"),
        ("If-Match" = Option<String>, Header, description = "The ETag it was read with"),
    ),
    request_body = EditTitleRequest,
    responses(
        (status = 200, description = "The title as it now is", body = TitleView),
        (status = 404, description = "No such title", body = Problem),
        (status = 409, description = "Changed since it was read", body = Problem),
        (status = 422, description = "A value that will not do", body = Problem),
    ),
)]
pub async fn edit_one_title(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(title_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<EditTitleRequest>,
) -> Result<Response, Problem> {
    let id = title_id
        .parse::<TitleId>()
        .map_err(|_| Problem::invalid_parameter("title_id", "not a valid identifier"))?;
    let expected = if_match(&headers)?;
    let kind = request
        .kind
        .as_deref()
        .map(|raw| {
            raw.parse::<TitleKind>()
                .map_err(|_| refused_value("a kind of work"))
        })
        .transpose()?;
    let display = request
        .display_title
        .as_deref()
        .map(|value| name(value, "display_title", MAX_NAME))
        .transpose()?;
    let sort = request
        .sort_title
        .as_deref()
        .map(|value| name(value, "sort_title", MAX_NAME))
        .transpose()?;
    if let Field::Set(year) = request.release_year
        && !RELEASE_YEARS.contains(&year)
    {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "release_year must be from 1870 to 2200; null clears it",
        ));
    }
    let outcome = edit_title(
        state.database().pool(),
        id,
        TitleEdit {
            kind: kind.as_ref().map(TitleKind::as_str),
            display_title: display.as_deref(),
            sort_title: sort.as_deref(),
            release_year: request.release_year.copied(),
        },
        expected,
        &user.username,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not edit a title");
        unavailable("the title could not be changed")
    })?;
    answer(outcome, "title", &title_id, |r| r.version, TitleView::from)
}

// --- editions ----------------------------------------------------------------

/// A change to an edition. Fields left out are left alone.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditEditionRequest {
    /// What this release is called.
    #[serde(default, deserialize_with = "not_null")]
    pub display_name: Option<String>,
    /// Who published it; null clears it.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub publisher: Field<String>,
    /// Which region it was sold in; null clears it.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub region: Field<String>,
}

/// One edition.
///
/// # Errors
///
/// `NOT_FOUND` if no such edition exists.
#[utoipa::path(
    get,
    path = "/api/v1/editions/{edition_id}",
    tag = "catalog",
    description = "Fetch one edition, its version as its ETag.",
    params(("edition_id" = String, Path, description = "Edition identifier")),
    responses(
        (status = 200, description = "The edition", body = EditionView),
        (status = 404, description = "No such edition", body = Problem),
    ),
)]
pub async fn get_one_edition(
    State(state): State<ApiState>,
    Path(edition_id): Path<String>,
) -> Result<Response, Problem> {
    let id = edition_id
        .parse::<EditionId>()
        .map_err(|_| Problem::invalid_parameter("edition_id", "not a valid identifier"))?;
    let edition = get_edition(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read an edition");
            unavailable("the edition could not be read")
        })?
        .ok_or_else(|| Problem::not_found("edition", &edition_id))?;
    let version = edition.version;
    Ok(with_etag(EditionView::from(edition), version))
}

/// Change an edition.
///
/// # Errors
///
/// `NOT_FOUND`, `VALIDATION_FAILED`, `RESOURCE_VERSION_CONFLICT`.
#[utoipa::path(
    patch,
    path = "/api/v1/editions/{edition_id}",
    tag = "catalog",
    description = "Change the fields given of an edition, as a title is changed. Audited.",
    params(
        ("edition_id" = String, Path, description = "Edition identifier"),
        ("If-Match" = Option<String>, Header, description = "The ETag it was read with"),
    ),
    request_body = EditEditionRequest,
    responses(
        (status = 200, description = "The edition as it now is", body = EditionView),
        (status = 404, description = "No such edition", body = Problem),
        (status = 409, description = "Changed since it was read", body = Problem),
        (status = 422, description = "A value that will not do", body = Problem),
    ),
)]
pub async fn edit_one_edition(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(edition_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<EditEditionRequest>,
) -> Result<Response, Problem> {
    let id = edition_id
        .parse::<EditionId>()
        .map_err(|_| Problem::invalid_parameter("edition_id", "not a valid identifier"))?;
    let expected = if_match(&headers)?;
    let display = request
        .display_name
        .as_deref()
        .map(|value| name(value, "display_name", MAX_NAME))
        .transpose()?;
    let publisher = optional(&request.publisher, "publisher", MAX_PUBLISHER)?;
    let region = optional(&request.region, "region", MAX_REGION)?;
    let outcome = edit_edition(
        state.database().pool(),
        id,
        EditionEdit {
            display_name: display.as_deref(),
            publisher: publisher.change(),
            region: region.change(),
        },
        expected,
        &user.username,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not edit an edition");
        unavailable("the edition could not be changed")
    })?;
    answer(
        outcome,
        "edition",
        &edition_id,
        |r| r.version,
        EditionView::from,
    )
}

// --- disc sets ---------------------------------------------------------------

/// A change to a disc set. Fields left out are left alone.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditDiscSetRequest {
    /// What the set is called.
    #[serde(default, deserialize_with = "not_null")]
    pub name: Option<String>,
    /// What shape of set it is.
    #[serde(default, deserialize_with = "not_null")]
    pub set_kind: Option<String>,
    /// How many discs it should contain, at least 1; null clears it.
    #[serde(default)]
    #[schema(value_type = Option<i16>)]
    pub disc_count_expected: Field<i16>,
}

/// One disc set.
///
/// # Errors
///
/// `NOT_FOUND` if no such set exists.
#[utoipa::path(
    get,
    path = "/api/v1/disc-sets/{disc_set_id}",
    tag = "catalog",
    description = "Fetch one disc set, its version as its ETag.",
    params(("disc_set_id" = String, Path, description = "Disc set identifier")),
    responses(
        (status = 200, description = "The disc set", body = DiscSetView),
        (status = 404, description = "No such disc set", body = Problem),
    ),
)]
pub async fn get_one_disc_set(
    State(state): State<ApiState>,
    Path(disc_set_id): Path<String>,
) -> Result<Response, Problem> {
    let id = disc_set_id
        .parse::<DiscSetId>()
        .map_err(|_| Problem::invalid_parameter("disc_set_id", "not a valid identifier"))?;
    let set = get_disc_set(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a disc set");
            unavailable("the disc set could not be read")
        })?
        .ok_or_else(|| Problem::not_found("disc set", &disc_set_id))?;
    let version = set.version;
    Ok(with_etag(DiscSetView::from(set), version))
}

/// Change a disc set.
///
/// # Errors
///
/// `NOT_FOUND`, `VALIDATION_FAILED`, `RESOURCE_VERSION_CONFLICT`.
#[utoipa::path(
    patch,
    path = "/api/v1/disc-sets/{disc_set_id}",
    tag = "catalog",
    description = "Change the fields given of a disc set, as a title is changed. Audited.",
    params(
        ("disc_set_id" = String, Path, description = "Disc set identifier"),
        ("If-Match" = Option<String>, Header, description = "The ETag it was read with"),
    ),
    request_body = EditDiscSetRequest,
    responses(
        (status = 200, description = "The disc set as it now is", body = DiscSetView),
        (status = 404, description = "No such disc set", body = Problem),
        (status = 409, description = "Changed since it was read", body = Problem),
        (status = 422, description = "A value that will not do", body = Problem),
    ),
)]
pub async fn edit_one_disc_set(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(disc_set_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<EditDiscSetRequest>,
) -> Result<Response, Problem> {
    let id = disc_set_id
        .parse::<DiscSetId>()
        .map_err(|_| Problem::invalid_parameter("disc_set_id", "not a valid identifier"))?;
    let expected = if_match(&headers)?;
    let set_name = request
        .name
        .as_deref()
        .map(|value| name(value, "name", MAX_NAME))
        .transpose()?;
    let kind = request
        .set_kind
        .as_deref()
        .map(|raw| {
            raw.parse::<SetKind>()
                .map_err(|_| refused_value("a kind of set"))
        })
        .transpose()?;
    if let Field::Set(count) = request.disc_count_expected
        && count < 1
    {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "disc_count_expected must be at least 1; null clears it",
        ));
    }
    let outcome = edit_disc_set(
        state.database().pool(),
        id,
        DiscSetEdit {
            name: set_name.as_deref(),
            set_kind: kind.as_ref().map(SetKind::as_str),
            disc_count_expected: request.disc_count_expected.copied(),
        },
        expected,
        &user.username,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not edit a disc set");
        unavailable("the disc set could not be changed")
    })?;
    answer(
        outcome,
        "disc set",
        &disc_set_id,
        |r| r.version,
        DiscSetView::from,
    )
}

// --- discs -------------------------------------------------------------------

/// A change to a disc. Fields left out are left alone.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditDiscRequest {
    /// Which disc of the set it is, from 1; unique within the set.
    #[serde(default, deserialize_with = "not_null")]
    pub sequence_number: Option<i16>,
    /// What it is called, when it has a name of its own; null clears it.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub display_name: Field<String>,
    /// What kind of medium it is.
    #[serde(default, deserialize_with = "not_null")]
    pub media_family: Option<String>,
    /// Which region it plays in; null clears it.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub region: Field<String>,
}

/// Change a disc.
///
/// # Errors
///
/// `NOT_FOUND`, `VALIDATION_FAILED`, `CONFLICT` for a number already in the
/// set, `RESOURCE_VERSION_CONFLICT`.
#[utoipa::path(
    patch,
    path = "/api/v1/discs/{disc_id}",
    tag = "catalog",
    description = "Change the fields given of a disc, as a title is changed. Audited.",
    params(
        ("disc_id" = String, Path, description = "Disc identifier"),
        ("If-Match" = Option<String>, Header, description = "The ETag it was read with"),
    ),
    request_body = EditDiscRequest,
    responses(
        (status = 200, description = "The disc as it now is", body = DiscView),
        (status = 404, description = "No such disc", body = Problem),
        (status = 409, description = "Changed since it was read, or the number is taken", body = Problem),
        (status = 422, description = "A value that will not do", body = Problem),
    ),
)]
pub async fn edit_one_disc(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(disc_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<EditDiscRequest>,
) -> Result<Response, Problem> {
    let id = disc_id
        .parse::<DiscId>()
        .map_err(|_| Problem::invalid_parameter("disc_id", "not a valid identifier"))?;
    let expected = if_match(&headers)?;
    if request.sequence_number.is_some_and(|number| number < 1) {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "sequence_number must be at least 1",
        ));
    }
    let display = optional(&request.display_name, "display_name", MAX_NAME)?;
    let family = request
        .media_family
        .as_deref()
        .map(|raw| {
            raw.parse::<MediaFamily>()
                .map_err(|_| refused_value("a medium"))
        })
        .transpose()?;
    let region = optional(&request.region, "region", MAX_REGION)?;
    let outcome = edit_disc(
        state.database().pool(),
        id,
        DiscEdit {
            sequence_number: request.sequence_number,
            display_name: display.change(),
            media_family: family.as_ref().map(MediaFamily::as_str),
            region: region.change(),
        },
        expected,
        &user.username,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not edit a disc");
        unavailable("the disc could not be changed")
    })?;
    answer(outcome, "disc", &disc_id, |r| r.version, DiscView::from)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_MATCH, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn if_match_names_a_version_or_nothing() {
        assert_eq!(if_match(&HeaderMap::new()).unwrap(), None);
        assert_eq!(if_match(&headers("*")).unwrap(), None);
        assert_eq!(if_match(&headers("\"7\"")).unwrap(), Some(7));
        assert_eq!(if_match(&headers("W/\"7\"")).unwrap(), Some(7));
        for bad in ["7", "\"seven\"", "\"0\"", "\"-1\"", "\"7"] {
            assert!(if_match(&headers(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn null_clears_and_absence_leaves() {
        let absent: EditDiscRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.region, Field::Keep);
        let cleared: EditDiscRequest = serde_json::from_str(r#"{"region": null}"#).unwrap();
        assert_eq!(cleared.region, Field::Clear);
        let set: EditDiscRequest = serde_json::from_str(r#"{"region": "PAL"}"#).unwrap();
        assert_eq!(set.region, Field::Set("PAL".to_owned()));
        assert!(serde_json::from_str::<EditDiscRequest>(r#"{"colour": "red"}"#).is_err());
        // A field that cannot be empty refuses null rather than ignoring it.
        assert!(serde_json::from_str::<EditDiscRequest>(r#"{"media_family": null}"#).is_err());
        assert!(serde_json::from_str::<EditTitleRequest>(r#"{"display_title": null}"#).is_err());
        let left: EditTitleRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(left.display_title, None);
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(name("  Doom ", "display_title", 500).unwrap(), "Doom");
        assert!(name("   ", "display_title", 500).is_err());
        assert!(name(&"x".repeat(501), "display_title", 500).is_err());
        assert!(
            name("Doom\u{0}", "display_title", 500).is_err(),
            "NUL, which PostgreSQL refuses"
        );
        assert_eq!(optional(&Field::Keep, "region", 100).unwrap(), Field::Keep);
        assert_eq!(
            optional(&Field::Clear, "region", 100).unwrap(),
            Field::Clear
        );
        assert_eq!(
            optional(&Field::Set(" PAL ".to_owned()), "region", 100).unwrap(),
            Field::Set("PAL".to_owned())
        );
        assert!(optional(&Field::Set(String::new()), "region", 100).is_err());
    }
}
