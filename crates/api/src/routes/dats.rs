// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Preservation DATs (docs/plans/dat-matching.md H2): an administrator
//! loads a Logiqx XML DAT (Redump's, No-Intro's) as the request body; its
//! games are matched against every hashed artifact at once, and every
//! artifact hashed later is matched against it. Tangible never fetches a
//! DAT itself.
//!
//! A DAT with the name of one already loaded replaces it, so loading a
//! newer one updates rather than duplicates; the same file loaded again
//! changes nothing.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tangible_db::dats::{
    DatRecord, NewDat, NewGame, NewRom, delete_dat, get_dat, list_dats, load_dat, rematch,
};
use tangible_image::dat::{self, DatError, DatFile, MAX_DAT_BYTES};
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// Warnings a load answers with; the rest are counted.
const MAX_WARNINGS_SHOWN: usize = 50;

fn unavailable(what: &'static str) -> impl Fn(tangible_db::DbError) -> Problem {
    move |error| {
        tracing::error!(error = ?error, "{what}");
        Problem::new(
            ErrorCode::StorageUnavailable,
            format!("{what}; see server logs"),
        )
    }
}

fn parse_id(raw: &str) -> Result<uuid::Uuid, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("dat_id", "not a valid identifier"))
}

/// A loaded DAT.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DatView {
    /// Its id.
    pub id: String,
    /// Its header name, such as `Sony - PlayStation`, which identifies it.
    pub name: String,
    /// Its description.
    pub description: Option<String>,
    /// Its version, often a date.
    pub version: Option<String>,
    /// Who made it.
    pub author: Option<String>,
    /// Where it is from.
    pub homepage: Option<String>,
    /// The SHA-256 of the file loaded.
    pub source_sha256: String,
    /// How many games it lists.
    pub game_count: i32,
    /// How many files, all games together.
    pub rom_count: i32,
    /// Who loaded it.
    pub loaded_by: String,
    /// When.
    pub loaded_at: String,
    /// How many artifacts it identifies, exactly or in part.
    pub matched_artifacts: i64,
}

impl From<DatRecord> for DatView {
    fn from(record: DatRecord) -> Self {
        Self {
            id: record.id.to_string(),
            name: record.name,
            description: record.description,
            version: record.version,
            author: record.author,
            homepage: record.homepage,
            source_sha256: record.source_sha256,
            game_count: record.game_count,
            rom_count: record.rom_count,
            loaded_by: record.loaded_by,
            loaded_at: record.loaded_at.format(&Rfc3339).unwrap_or_default(),
            matched_artifacts: record.matched_artifacts,
        }
    }
}

/// What loading a DAT came to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DatLoadView {
    /// The DAT as loaded.
    pub dat: DatView,
    /// Whether it replaced a DAT of the same name.
    pub replaced: bool,
    /// Whether this very file was already loaded, so nothing changed.
    pub unchanged: bool,
    /// What in the file was left out and why, the first fifty.
    pub warnings: Vec<String>,
    /// How many warnings in all.
    pub warning_count: usize,
}

/// Every loaded DAT.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DatList {
    /// By name.
    pub items: Vec<DatView>,
}

fn dat_problem(error: &DatError) -> Problem {
    Problem::new(ErrorCode::DatInvalid, error.to_string())
}

/// The DAT, as rows to load.
fn rows<'a>(file: &'a DatFile, source_sha256: &'a str, loaded_by: &'a str) -> NewDat<'a> {
    NewDat {
        name: &file.header.name,
        description: file.header.description.as_deref(),
        version: file.header.version.as_deref(),
        author: file.header.author.as_deref(),
        homepage: file.header.homepage.as_deref(),
        url: file.header.url.as_deref(),
        source_sha256,
        loaded_by,
        games: file
            .games
            .iter()
            .map(|game| NewGame {
                name: &game.name,
                description: game.description.as_deref(),
                category: game.category.as_deref(),
                roms: game
                    .roms
                    .iter()
                    .map(|rom| NewRom {
                        name: &rom.name,
                        size: rom.size,
                        crc32: rom.crc32.as_deref(),
                        md5: rom.md5.as_deref(),
                        sha1: rom.sha1.as_deref(),
                        sha256: rom.sha256.as_deref(),
                        status: rom.status.as_deref(),
                        is_cue: rom.is_cue(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Load a DAT.
///
/// # Errors
///
/// `DAT_INVALID` for a body that is not a Logiqx XML DAT this server reads.
#[utoipa::path(
    post,
    path = "/api/v1/dats",
    tag = "library",
    description = "Load a Logiqx XML DAT, such as Redump's or No-Intro's, sent as the request \
                   body (up to 64 MiB, not zipped). One with the name of a DAT already loaded \
                   replaces it. Its games are matched against every artifact's preservation \
                   hashes at once. Administrators only.",
    request_body(content = String, content_type = "application/xml",
                 description = "The DAT's XML"),
    responses(
        (status = 201, description = "Loaded", body = DatLoadView),
        (status = 200, description = "This very file was already loaded", body = DatLoadView),
        (status = 422, description = "Not a DAT this server reads", body = Problem),
    ),
)]
pub async fn load(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    body: Bytes,
) -> Result<(StatusCode, Json<DatLoadView>), Problem> {
    let source_sha256 = hex::encode(Sha256::digest(&body));
    // A large DAT takes a while to read; not on the request threads.
    let parsed = tokio::task::spawn_blocking(move || dat::parse(&body))
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "reading a DAT stopped");
            Problem::new(ErrorCode::Internal, "the DAT could not be read")
        })?
        .map_err(|error| dat_problem(&error))?;
    let warning_count = parsed.warnings.len();
    let warnings: Vec<String> = parsed
        .warnings
        .iter()
        .take(MAX_WARNINGS_SHOWN)
        .map(ToString::to_string)
        .collect();
    let pool = state.database().pool();

    let standing = list_dats(pool)
        .await
        .map_err(unavailable("the DATs could not be read"))?
        .into_iter()
        .find(|record| record.name == parsed.header.name);
    if let Some(record) = standing.filter(|record| record.source_sha256 == source_sha256) {
        return Ok((
            StatusCode::OK,
            Json(DatLoadView {
                dat: record.into(),
                replaced: false,
                unchanged: true,
                warnings,
                warning_count,
            }),
        ));
    }

    let loaded = load_dat(pool, &rows(&parsed, &source_sha256, &user.username))
        .await
        .map_err(unavailable("the DAT could not be stored"))?;
    let matches = rematch(pool, None, Some(loaded.id))
        .await
        .map_err(unavailable("the DAT was stored but could not be matched"))?;
    tracing::info!(
        dat_id = %loaded.id,
        name = %parsed.header.name,
        games = parsed.games.len(),
        files = parsed.rom_count(),
        replaced = loaded.replaced,
        matches,
        by = %user.username,
        "DAT loaded"
    );
    let record = get_dat(pool, loaded.id)
        .await
        .map_err(unavailable("the DAT could not be read"))?
        .ok_or_else(|| Problem::not_found("dat", &loaded.id.to_string()))?;
    Ok((
        StatusCode::CREATED,
        Json(DatLoadView {
            dat: record.into(),
            replaced: loaded.replaced,
            unchanged: false,
            warnings,
            warning_count,
        }),
    ))
}

/// Every loaded DAT.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` when the database cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/dats",
    tag = "library",
    description = "Every loaded DAT, by name, with how many artifacts each identifies.",
    responses((status = 200, description = "The DATs", body = DatList)),
)]
pub async fn list(State(state): State<ApiState>) -> Result<Json<DatList>, Problem> {
    let items = list_dats(state.database().pool())
        .await
        .map_err(unavailable("the DATs could not be read"))?
        .into_iter()
        .map(DatView::from)
        .collect();
    Ok(Json(DatList { items }))
}

/// Remove a DAT.
///
/// # Errors
///
/// `NOT_FOUND` for no such DAT.
#[utoipa::path(
    delete,
    path = "/api/v1/dats/{dat_id}",
    tag = "library",
    description = "Remove a DAT, and every match it made. Nothing else changes: matches \
                   never renamed or changed anything. Administrators only.",
    params(("dat_id" = String, Path, description = "DAT identifier")),
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "No such DAT", body = Problem),
    ),
)]
pub async fn remove(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(dat_id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_id(&dat_id)?;
    let removed = delete_dat(state.database().pool(), id, &user.username)
        .await
        .map_err(unavailable("the DAT could not be removed"))?;
    if !removed {
        return Err(Problem::not_found("dat", &id.to_string()));
    }
    tracing::info!(dat_id = %id, by = %user.username, "DAT removed");
    Ok(StatusCode::NO_CONTENT)
}

/// The DAT routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/dats",
            get(list)
                .post(load)
                .layer(DefaultBodyLimit::max(MAX_DAT_BYTES)),
        )
        .route("/dats/{dat_id}", delete(remove))
}
