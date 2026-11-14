// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Metadata search through the real router, with MusicBrainz, TMDB, IGDB
//! and Twitch played by a fake on a loopback port answering the fixtures
//! (docs/plans/requests.md Q2). The providers are integrations an
//! administrator makes through the API, keys sealed.
//!
//! There may be one integration of each provider kind, and the database is
//! shared, so these tests take turns and remove the providers first.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::metadata::{Endpoints, Metadata};
use tangible_api::secrets::SecretKey;
use tangible_api::{ApiState, router};
use tangible_domain::Role;
use tangible_integrations::metadata::igdb;
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy, Url};
use tower::ServiceExt as _;

mod support;

const TMDB_TOKEN: &str = "tmdb-read-access-token-for-the-fake";
const CLIENT_ID: &str = "fake-twitch-client-id";
const CLIENT_SECRET: &str = "fake-twitch-client-secret";
/// The token in twitch-token.json.
const TWITCH_TOKEN: &str = "jostpf5q0uzmxmkba9iyug38kjtgh";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../fixtures/tool-output/metadata/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// What the fake was asked: method, path and query, and the headers.
#[derive(Debug, Clone)]
struct Asked {
    what: String,
    headers: HeaderMap,
    body: String,
}

type Shared = Arc<Mutex<Vec<Asked>>>;

async fn answer(
    State(shared): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    let what = format!(
        "{method} {}{}",
        uri.path(),
        uri.query().map(|q| format!("?{q}")).unwrap_or_default()
    );
    shared.lock().unwrap().push(Asked {
        what: what.clone(),
        headers: headers.clone(),
        body: body.clone(),
    });
    let bearer = |token: &str| {
        headers.get("authorization").and_then(|v| v.to_str().ok())
            == Some(&format!("Bearer {token}"))
    };
    let json = |status: StatusCode, text: String| {
        (status, [("content-type", "application/json")], text).into_response()
    };
    match (method.as_str(), uri.path()) {
        ("GET", "/ws/2/release") => {
            json(StatusCode::OK, fixture("musicbrainz-release-search.json"))
        }
        ("GET", path) if path.starts_with("/3/") => {
            if !bearer(TMDB_TOKEN) {
                return json(StatusCode::UNAUTHORIZED, fixture("tmdb-unauthorized.json"));
            }
            match path {
                "/3/search/movie" => json(StatusCode::OK, fixture("tmdb-search-movie.json")),
                "/3/search/tv" => json(StatusCode::OK, fixture("tmdb-search-tv.json")),
                "/3/find/tt0083658" => json(StatusCode::OK, fixture("tmdb-find-imdb.json")),
                "/3/configuration" => json(StatusCode::OK, fixture("tmdb-configuration.json")),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
        ("POST", "/oauth2/token") => {
            // The values here need no decoding: letters, digits, - and _.
            let form: Vec<(&str, &str)> = body
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .collect();
            let has = |k: &str, v: &str| form.iter().any(|(a, b)| *a == k && *b == v);
            if has("client_id", CLIENT_ID)
                && has("client_secret", CLIENT_SECRET)
                && has("grant_type", "client_credentials")
            {
                json(StatusCode::OK, fixture("twitch-token.json"))
            } else {
                json(StatusCode::FORBIDDEN, fixture("twitch-token-refused.json"))
            }
        }
        ("POST", "/v4/games") => {
            let client = headers.get("client-id").and_then(|v| v.to_str().ok());
            if client != Some(CLIENT_ID) || !bearer(TWITCH_TOKEN) {
                return json(StatusCode::UNAUTHORIZED, fixture("igdb-unauthorized.json"));
            }
            json(StatusCode::OK, fixture("igdb-games.json"))
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn fake() -> (SocketAddr, Shared) {
    let shared: Shared = Arc::new(Mutex::new(Vec::new()));
    let app = axum::Router::new()
        .fallback(answer)
        .with_state(Arc::clone(&shared));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, shared)
}

static TURN: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

struct Harness {
    pool: PgPool,
    state: ApiState,
    admin: support::Session,
    asked: Shared,
    _turn: tokio::sync::MutexGuard<'static, ()>,
}

async fn remove_providers(pool: &PgPool) {
    sqlx::query(
        "DELETE FROM integration_secrets WHERE integration_id IN
             (SELECT id FROM integrations WHERE kind IN ('musicbrainz', 'tmdb', 'igdb'))",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM integrations WHERE kind IN ('musicbrainz', 'tmdb', 'igdb')")
        .execute(pool)
        .await
        .unwrap();
}

async fn harness() -> Harness {
    let turn = TURN
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let database = support::database().await;
    let pool = database.pool().clone();
    remove_providers(&pool).await;
    let (address, asked) = fake().await;
    let base = |path: &str| Url::parse(&format!("http://{address}{path}")).unwrap();
    let client = OutboundClient::new(
        OutboundPolicy::for_integrations(vec!["127.0.0.1/32".parse().unwrap()], None),
        tangible_api::metadata::USER_AGENT,
    )
    .unwrap();
    let metadata = Metadata::new(
        client,
        Endpoints {
            musicbrainz: base("/"),
            tmdb: base("/3/"),
            igdb: igdb::Endpoints {
                api: base("/v4/"),
                token: base("/oauth2/token"),
            },
        },
    );
    let key = SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap();
    let state = ApiState::new(database)
        .with_secret_key(key)
        .with_metadata(metadata);
    Harness {
        admin: support::administrator(&pool).await,
        pool,
        state,
        asked,
        _turn: turn,
    }
}

impl Harness {
    async fn send(
        &self,
        session: &support::Session,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let mut request = request
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        session.apply(&mut request);
        let response = router(self.state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn search(&self, session: &support::Session, query: &str) -> (StatusCode, Value) {
        self.send(
            session,
            Method::GET,
            &format!("/api/v1/metadata/search?{query}"),
            None,
        )
        .await
    }

    async fn make(&self, kind: &str, secrets: Value) -> String {
        let (status, made) = self
            .send(
                &self.admin,
                Method::POST,
                "/api/v1/integrations",
                Some(json!({
                    "kind": kind,
                    "name": format!("{kind} {}", uuid::Uuid::now_v7().simple()),
                    "settings": {},
                    "secrets": secrets,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        made["id"].as_str().unwrap().to_owned()
    }

    async fn make_all(&self) -> [String; 3] {
        [
            self.make("musicbrainz", json!({})).await,
            self.make("tmdb", json!({ "read_access_token": TMDB_TOKEN }))
                .await,
            self.make(
                "igdb",
                json!({ "client_id": CLIENT_ID, "client_secret": CLIENT_SECRET }),
            )
            .await,
        ]
    }

    fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }

    async fn done(self) {
        remove_providers(&self.pool).await;
    }
}

fn source<'a>(answer: &'a Value, provider: &str) -> &'a Value {
    answer["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["provider"] == provider)
        .unwrap_or_else(|| panic!("no source {provider} in {answer}"))
}

fn providers(answer: &Value) -> Vec<&str> {
    answer["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| source["provider"].as_str().unwrap())
        .collect()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unconfigured_provider_is_named_and_the_dats_still_answer() {
    let harness = harness().await;
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;
    let (status, answer) = harness.search(&viewer, "q=zelda&kind=game").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(providers(&answer), ["dat", "igdb"]);
    assert_eq!(source(&answer, "dat")["state"], "answered");
    let igdb = source(&answer, "igdb");
    assert_eq!(igdb["state"], "unconfigured");
    assert!(
        igdb["detail"]
            .as_str()
            .unwrap()
            .contains("Integrations page")
    );
    assert!(harness.asked().is_empty(), "nothing reached outside");

    // An IMDb ID needs TMDB, and says so.
    let (_, answer) = harness.search(&viewer, "q=tt0083658").await;
    assert_eq!(providers(&answer), ["tmdb"]);
    assert!(
        source(&answer, "tmdb")["detail"]
            .as_str()
            .unwrap()
            .contains("IMDb ID is looked up through TMDB")
    );
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn each_kind_asks_its_providers_and_answers_what_a_request_takes() {
    let harness = harness().await;
    harness.make_all().await;
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;

    // Music: MusicBrainz, named as it asks, with dismax.
    let (status, answer) = harness
        .search(&viewer, "q=pink+floyd+dark+side&kind=music")
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(providers(&answer), ["musicbrainz"]);
    let first = &answer["candidates"][0];
    assert_eq!(first["provider"], "musicbrainz");
    assert_eq!(first["kind"], "music");
    assert_eq!(first["details"]["artist"], "E.S.P");
    assert_eq!(first["refs"][0]["provider"], "musicbrainz");
    let asked = harness.asked();
    let musicbrainz = asked
        .iter()
        .find(|a| a.what.starts_with("GET /ws/2/release"))
        .unwrap();
    assert!(
        musicbrainz.what.contains("dismax=true"),
        "{}",
        musicbrainz.what
    );
    let agent = musicbrainz.headers["user-agent"].to_str().unwrap();
    assert!(agent.starts_with("Tangible/") && agent.contains("github.com/digital-grease/tangible"));

    // Films: TMDB, signed with the token, never in the address.
    let (_, answer) = harness.search(&viewer, "q=blade+runner&kind=movie").await;
    assert_eq!(providers(&answer), ["tmdb"]);
    assert_eq!(answer["candidates"][0]["title"], "Blade Runner");
    assert_eq!(
        answer["candidates"][0]["refs"][0]["external_id"],
        "movie/78"
    );
    for asked in harness.asked().iter().filter(|a| a.what.contains("/3/")) {
        assert!(
            !asked.what.contains(TMDB_TOKEN),
            "the token is never in a URL"
        );
    }

    // Games: the DATs and IGDB, through a Twitch token asked for once.
    let (_, answer) = harness.search(&viewer, "q=zelda&kind=game").await;
    assert_eq!(providers(&answer), ["dat", "igdb"]);
    assert_eq!(source(&answer, "igdb")["state"], "answered");
    assert_eq!(source(&answer, "igdb")["count"], 3);
    let igdb = answer["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["provider"] == "igdb")
        .unwrap();
    assert_eq!(
        igdb["details"]["platform"],
        "Super Nintendo Entertainment System, Game Boy Advance"
    );
    let asked = harness.asked();
    let token = asked
        .iter()
        .find(|a| a.what == "POST /oauth2/token")
        .unwrap();
    assert!(
        !token.what.contains(CLIENT_SECRET),
        "the secret goes in the body"
    );
    assert!(token.body.contains("client_secret="));
    let query = asked.iter().find(|a| a.what == "POST /v4/games").unwrap();
    assert!(
        query.body.starts_with("search \"zelda\";"),
        "{}",
        query.body
    );

    // A second, different game search reuses the token.
    harness.search(&viewer, "q=ocarina&kind=game").await;
    let tokens = harness
        .asked()
        .iter()
        .filter(|a| a.what == "POST /oauth2/token")
        .count();
    assert_eq!(tokens, 1, "the token is kept");

    // An IMDb address: TMDB's find alone, the IMDb ID kept as a reference.
    let (_, answer) = harness
        .search(
            &viewer,
            "q=https%3A%2F%2Fwww.imdb.com%2Ftitle%2Ftt0083658%2F",
        )
        .await;
    assert_eq!(providers(&answer), ["tmdb"]);
    let refs = &answer["candidates"][0]["refs"];
    assert_eq!(
        refs[1],
        json!({ "provider": "imdb", "external_id": "tt0083658" })
    );
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_answer_is_kept_for_the_same_query() {
    let harness = harness().await;
    harness.make_all().await;
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;
    let word = uuid::Uuid::now_v7().simple().to_string();
    harness
        .search(&viewer, &format!("q=kept+{word}&kind=movie"))
        .await;
    let before = harness.asked().len();
    // The same words, in another case.
    let (_, again) = harness
        .search(
            &viewer,
            &format!("q=KEPT+{}&kind=movie", word.to_uppercase()),
        )
        .await;
    assert_eq!(source(&again, "tmdb")["state"], "answered");
    assert_eq!(harness.asked().len(), before, "answered from what was kept");
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_failing_provider_is_named_and_the_others_still_answer() {
    let harness = harness().await;
    harness.make("musicbrainz", json!({})).await;
    harness
        .make("tmdb", json!({ "read_access_token": "wrong" }))
        .await;
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;
    let (status, answer) = harness.search(&viewer, "q=failing+provider+query").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let tmdb = source(&answer, "tmdb");
    assert_eq!(tmdb["state"], "failed");
    assert_eq!(tmdb["error_code"], "INTEGRATION_AUTH_FAILED");
    assert!(
        tmdb["detail"]
            .as_str()
            .unwrap()
            .contains("Read Access Token")
    );
    assert_eq!(source(&answer, "musicbrainz")["state"], "answered");
    assert_eq!(source(&answer, "igdb")["state"], "unconfigured");
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_administrator_tries_each_provider_and_there_is_one_of_each() {
    let harness = harness().await;
    let ids = harness.make_all().await;
    for id in &ids {
        let (status, tried) = harness
            .send(
                &harness.admin,
                Method::POST,
                &format!("/api/v1/integrations/{id}/test"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{tried}");
        assert_eq!(tried["ok"], true, "{tried}");
    }
    // A second TMDB is refused: it is one service.
    let (status, problem) = harness
        .send(
            &harness.admin,
            Method::POST,
            "/api/v1/integrations",
            Some(json!({
                "kind": "tmdb",
                "name": "another",
                "settings": {},
                "secrets": { "read_access_token": "x" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("tmdb integration already")
    );

    // A secret is listed, never answered.
    let (_, listed) = harness
        .send(
            &harness.admin,
            Method::GET,
            &format!("/api/v1/integrations/{}", ids[2]),
            None,
        )
        .await;
    assert_eq!(
        listed["secrets"],
        json!([{ "field": "client_id", "set": true }, { "field": "client_secret", "set": true }])
    );
    assert!(!listed.to_string().contains(CLIENT_SECRET));

    // Turned off, it is named as off.
    let (status, _) = harness
        .send(
            &harness.admin,
            Method::PATCH,
            &format!("/api/v1/integrations/{}", ids[0]),
            Some(json!({ "enabled": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, answer) = harness
        .search(&harness.admin, "q=off+provider&kind=music")
        .await;
    assert_eq!(source(&answer, "musicbrainz")["state"], "off");
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_search_needs_words_and_a_known_kind() {
    let harness = harness().await;
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;
    for query in ["q=", "q=+++", "q=zelda&kind=book"] {
        let (status, problem) = harness.search(&viewer, query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {problem}");
        assert_eq!(problem["code"], "INVALID_PARAMETER");
    }
    let long = "x".repeat(201);
    let (status, _) = harness.search(&viewer, &format!("q={long}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    harness.done().await;
}
