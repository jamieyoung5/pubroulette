//! Local dev server: every handler mounted at the paths the deployed
//! functions use, so the whole API runs as one process.
//!
//! Needs a `.env` with `GEOAPIFY_API_KEY`. Run with
//! `cargo run --example devserver` and it listens on port 3000.

use axum::body::Bytes;
use axum::extract::{Path, Query};
use axum::http::HeaderMap;
use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use pubroulette::auth;
use pubroulette::places::{GeoapifyConfig, Mode};
use pubroulette::rooms::{self, RoomError, RoomSettings};
use pubroulette::roulette::{AppError, PlaceFilters, get_random_crawl, get_random_pub};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tower_http::cors::CorsLayer;
use validator::Validate;

#[derive(Debug, Deserialize, Validate)]
struct PlayParams {
    #[validate(range(min = -90.0, max = 90.0))]
    latitude: f64,
    #[validate(range(min = -180.0, max = 180.0))]
    longitude: f64,
    #[validate(range(min = 1, max = 5000))]
    radius: i16,
    #[validate(range(min = 0.0, max = 360.0))]
    bearing: Option<f64>,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    visited: Vec<String>,
    #[serde(default)]
    beer_garden: bool,
}

#[derive(Debug, Deserialize, Validate)]
struct CrawlParams {
    #[validate(range(min = -90.0, max = 90.0))]
    latitude: f64,
    #[validate(range(min = -180.0, max = 180.0))]
    longitude: f64,
    #[validate(range(min = 1, max = 5000))]
    radius: i16,
    #[validate(range(min = 0.0, max = 360.0))]
    bearing: Option<f64>,
    #[validate(range(min = 1, max = 5))]
    num_pubs: i16,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    visited: Vec<String>,
    #[serde(default)]
    beer_garden: bool,
}

type ApiResult = Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)>;

fn json_error(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn map_app_error(e: AppError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = e.to_response();
    json_error(
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        message,
    )
}

fn parse_and_validate<T: serde::de::DeserializeOwned + Validate>(
    body: &[u8],
) -> Result<T, (StatusCode, Json<serde_json::Value>)> {
    let params: T = serde_json::from_slice(body)
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")))?;
    params
        .validate()
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("Validation failed: {e}")))?;
    Ok(params)
}

fn load_config() -> Result<GeoapifyConfig, (StatusCode, Json<serde_json::Value>)> {
    GeoapifyConfig::from_env().map_err(|e| json_error(StatusCode::INTERNAL_SERVER_ERROR, e))
}

async fn play_handler(body: Bytes) -> ApiResult {
    let params = parse_and_validate::<PlayParams>(&body)?;
    let config = load_config()?;

    let filters = PlaceFilters {
        beer_garden: params.beer_garden,
    };
    let resp = get_random_pub(
        &config,
        params.latitude,
        params.longitude,
        params.radius,
        params.bearing,
        params.mode,
        &params.visited,
        &filters,
    )
    .await
    .map_err(map_app_error)?;

    Ok(Json(serde_json::to_value(resp).unwrap()))
}

fn map_room_error(e: RoomError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = e.to_response();
    json_error(
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        message,
    )
}

/// Body for `POST /api/rooms`. Mints `device_id` like the deployed handler,
/// so the frontend doesn't need a dev-only branch.
#[derive(Debug, Deserialize)]
struct CreateRoomBody {
    name: String,
    settings: RoomSettings,
}

#[derive(Debug, Deserialize)]
struct JoinRoomBody {
    name: String,
}

/// The create, join and rejoin success shape. The frontend destructures
/// `{ room, token, device_id }`, so every field has to stay.
#[derive(Debug, Serialize)]
struct RoomWithToken<'a> {
    room: &'a rooms::Room,
    token: String,
    device_id: &'a str,
}

/// Verify the bearer token against the room code and return its `did`, so
/// host-only actions never trust a body field.
fn require_bearer(
    headers: &HeaderMap,
    expected_code: &str,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            json_error(
                StatusCode::UNAUTHORIZED,
                "This action requires a room session.",
            )
        })?;
    let token = auth_header
        .strip_prefix("Bearer ")
        .or_else(|| auth_header.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            json_error(
                StatusCode::UNAUTHORIZED,
                "This action requires a room session.",
            )
        })?;
    let secret = auth::load_secret().map_err(|_| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Authentication is temporarily unavailable.",
        )
    })?;
    let claims = auth::verify(secret, token, expected_code, chrono::Utc::now().timestamp())
        .map_err(|_| {
            json_error(
                StatusCode::UNAUTHORIZED,
                "Your room session is invalid or expired. Please rejoin.",
            )
        })?;
    Ok(claims.did)
}

/// Mint a token for `(room_code, device_id)` after a create or join.
fn mint_token(
    room_code: &str,
    device_id: &str,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let secret = auth::load_secret().map_err(|_| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Authentication is temporarily unavailable.",
        )
    })?;
    auth::mint(secret, room_code, device_id, chrono::Utc::now().timestamp()).map_err(|_| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Authentication is temporarily unavailable.",
        )
    })
}

async fn rooms_create_handler(body: Bytes) -> ApiResult {
    let parsed: CreateRoomBody = serde_json::from_slice(&body)
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")))?;
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    let device_id = uuid::Uuid::new_v4().to_string();
    let room = rooms::create_room(&mut conn, device_id.clone(), parsed.name, parsed.settings)
        .await
        .map_err(map_room_error)?;
    let token = mint_token(&room.code, &device_id)?;
    Ok(Json(
        serde_json::to_value(RoomWithToken {
            room: &room,
            token,
            device_id: &device_id,
        })
        .unwrap(),
    ))
}

async fn rooms_get_handler(Path(code): Path<String>) -> ApiResult {
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    let code = code.to_ascii_uppercase();
    // Resolve expired rounds here too, so local dev exercises the same path.
    let room = rooms::resolve_room_if_expired(&mut conn, &code)
        .await
        .map_err(map_room_error)?;
    Ok(Json(serde_json::to_value(room).unwrap()))
}

async fn rooms_join_handler(Path(code): Path<String>, body: Bytes) -> ApiResult {
    let parsed: JoinRoomBody = serde_json::from_slice(&body)
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")))?;
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    let code = code.to_ascii_uppercase();
    let device_id = uuid::Uuid::new_v4().to_string();
    let room = rooms::join_room(&mut conn, &code, device_id.clone(), parsed.name)
        .await
        .map_err(map_room_error)?;
    let token = mint_token(&room.code, &device_id)?;
    Ok(Json(
        serde_json::to_value(RoomWithToken {
            room: &room,
            token,
            device_id: &device_id,
        })
        .unwrap(),
    ))
}

async fn rooms_start_handler(
    Path(code): Path<String>,
    headers: HeaderMap,
    _body: Bytes,
) -> ApiResult {
    let code = code.to_ascii_uppercase();
    let did = require_bearer(&headers, &code)?;
    let geo = load_config()?;
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    let room = rooms::start_room(&mut conn, &geo, &code, &did)
        .await
        .map_err(map_room_error)?;
    Ok(Json(serde_json::to_value(room).unwrap()))
}

async fn rooms_advance_handler(
    Path(code): Path<String>,
    headers: HeaderMap,
    _body: Bytes,
) -> ApiResult {
    let code = code.to_ascii_uppercase();
    let did = require_bearer(&headers, &code)?;
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    let room = rooms::advance_room(&mut conn, &code, &did)
        .await
        .map_err(map_room_error)?;
    Ok(Json(serde_json::to_value(room).unwrap()))
}

async fn rooms_leave_handler(
    Path(code): Path<String>,
    headers: HeaderMap,
    _body: Bytes,
) -> ApiResult {
    let code = code.to_ascii_uppercase();
    let did = require_bearer(&headers, &code)?;
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    match rooms::leave_room(&mut conn, &code, &did).await {
        Ok(Some(room)) => Ok(Json(serde_json::to_value(room).unwrap())),
        Ok(None) => Ok(Json(serde_json::json!({"deleted": true}))),
        Err(e) => Err(map_room_error(e)),
    }
}

#[derive(Debug, Deserialize)]
struct VoteBody {
    #[serde(default)]
    approvals: Vec<String>,
}

async fn rooms_vote_handler(
    Path(code): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let code = code.to_ascii_uppercase();
    let did = require_bearer(&headers, &code)?;
    let parsed: VoteBody = serde_json::from_slice(&body)
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")))?;
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    let room = rooms::vote_room(&mut conn, &code, &did, &parsed.approvals)
        .await
        .map_err(map_room_error)?;
    Ok(Json(serde_json::to_value(room).unwrap()))
}

async fn rooms_skip_round_handler(
    Path(code): Path<String>,
    headers: HeaderMap,
    _body: Bytes,
) -> ApiResult {
    let code = code.to_ascii_uppercase();
    let did = require_bearer(&headers, &code)?;
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    let room = rooms::skip_round(&mut conn, &code, &did)
        .await
        .map_err(map_room_error)?;
    Ok(Json(serde_json::to_value(room).unwrap()))
}

async fn rooms_rejoin_handler(
    Path(code): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    // `device_id` comes from the token, never the body, so only the holder
    // of a signed token for this room can mint a new one. Expiry is ignored.
    let code = code.to_ascii_uppercase();
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            json_error(
                StatusCode::UNAUTHORIZED,
                "This action requires a room session.",
            )
        })?;
    let token = auth_header
        .strip_prefix("Bearer ")
        .or_else(|| auth_header.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            json_error(
                StatusCode::UNAUTHORIZED,
                "This action requires a room session.",
            )
        })?;
    let claims = auth::verify_with_rotation_allow_expired(token, &code).map_err(|_| {
        json_error(
            StatusCode::UNAUTHORIZED,
            "Your room session is invalid or expired. Please rejoin.",
        )
    })?;

    let parsed: JoinRoomBody = serde_json::from_slice(&body)
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")))?;
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    let device_id = claims.did.clone();
    let room = rooms::rejoin_room(&mut conn, &code, device_id.clone(), parsed.name)
        .await
        .map_err(map_room_error)?;
    let token = mint_token(&room.code, &device_id)?;
    Ok(Json(
        serde_json::to_value(RoomWithToken {
            room: &room,
            token,
            device_id: &device_id,
        })
        .unwrap(),
    ))
}

async fn rooms_next_round_handler(
    Path(code): Path<String>,
    headers: HeaderMap,
    _body: Bytes,
) -> ApiResult {
    let code = code.to_ascii_uppercase();
    let did = require_bearer(&headers, &code)?;
    let mut conn = rooms::connect().await.map_err(map_room_error)?;
    let room = rooms::open_next_round_for_code(&mut conn, &code, &did)
        .await
        .map_err(map_room_error)?;
    Ok(Json(serde_json::to_value(room).unwrap()))
}

async fn search_handler(Query(params): Query<HashMap<String, String>>) -> ApiResult {
    let q = params.get("q").map(|s| s.trim()).unwrap_or("");
    if q.chars().count() < 2 || q.chars().count() > 100 {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "Provide a 2-100 character `q` query parameter.",
        ));
    }
    let base = std::env::var("NOMINATIM_BASE_URL")
        .unwrap_or_else(|_| "https://nominatim.openstreetmap.org/search".to_owned());
    let resp = pubroulette::places::http_client()
        .get(base)
        .query(&[
            ("q", q),
            ("format", "json"),
            ("limit", "5"),
            ("addressdetails", "1"),
        ])
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| json_error(StatusCode::BAD_GATEWAY, format!("nominatim_error: {e}")))?;
    let value: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| json_error(StatusCode::BAD_GATEWAY, format!("nominatim_parse: {e}")))?;
    Ok(Json(value))
}

async fn crawl_handler(body: Bytes) -> ApiResult {
    let params = parse_and_validate::<CrawlParams>(&body)?;
    let config = load_config()?;

    let filters = PlaceFilters {
        beer_garden: params.beer_garden,
    };
    let resp = get_random_crawl(
        &config,
        params.num_pubs,
        params.latitude,
        params.longitude,
        params.radius,
        params.bearing,
        params.mode,
        &params.visited,
        &filters,
    )
    .await
    .map_err(map_app_error)?;

    Ok(Json(serde_json::to_value(resp).unwrap()))
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    pubroulette::init_tracing();

    if std::env::var("GEOAPIFY_API_KEY").is_err() {
        eprintln!("error: GEOAPIFY_API_KEY is not set.");
        eprintln!("       Copy .env.example to .env and add your key.");
        std::process::exit(1);
    }

    if std::env::var("REDIS_URL").is_err() {
        eprintln!("warning: REDIS_URL is not set, so /api/rooms endpoints will fail.");
        eprintln!("         Run `make redis-up` to start a local Redis container.");
    }

    let app = Router::new()
        .route("/api/play", post(play_handler))
        .route("/api/crawl", post(crawl_handler))
        .route("/api/search", get(search_handler))
        .route("/api/rooms", post(rooms_create_handler))
        .route("/api/rooms/{code}", get(rooms_get_handler))
        .route("/api/rooms/{code}/join", post(rooms_join_handler))
        .route("/api/rooms/{code}/start", post(rooms_start_handler))
        .route("/api/rooms/{code}/advance", post(rooms_advance_handler))
        .route("/api/rooms/{code}/leave", post(rooms_leave_handler))
        .route("/api/rooms/{code}/vote", post(rooms_vote_handler))
        .route(
            "/api/rooms/{code}/skip-round",
            post(rooms_skip_round_handler),
        )
        .route(
            "/api/rooms/{code}/next-round",
            post(rooms_next_round_handler),
        )
        .route("/api/rooms/{code}/rejoin", post(rooms_rejoin_handler))
        .layer(CorsLayer::permissive());

    let addr = "0.0.0.0:3000";
    eprintln!();
    eprintln!("  pubroulette dev server listening on http://localhost:3000");
    eprintln!();
    eprintln!("  Try:");
    eprintln!(
        r#"    curl -s localhost:3000/api/play -d '{{"latitude":55.946,"longitude":-3.205,"radius":500}}' | jq ."#
    );
    eprintln!();

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
