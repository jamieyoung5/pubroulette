//! Tests for the HTTP-to-handler glue: token extraction, room-code parity,
//! request-id propagation, expiry policy and rotation fallback.
//!
//! These drive `vercel::require_room_auth` and its allow-expired variant
//! against hand-built requests, with no Redis or Geoapify. Keeping the
//! assertions here means a later tidy-up of handler boilerplate can't quietly
//! move the auth boundary.

use http::{HeaderValue, Method};
use pubroulette::auth::{self, TOKEN_TTL_SECONDS};
use pubroulette::vercel::{
    self, Ctx, REQUEST_ID_HEADER, require_room_auth, require_room_auth_allow_expired,
};
use std::sync::Once;
use vercel_runtime::{Body, Request};

static SECRETS_INIT: Once = Once::new();

/// Pin a signing secret for the whole test process. The secret is read from
/// the environment once, on first auth call, so this has to win that race.
fn ensure_secrets() {
    SECRETS_INIT.call_once(|| {
        let current = "A".repeat(32);
        let previous = "B".repeat(32);
        // SAFETY: process-global mutation, but `Once` runs it before any
        // test reads the value.
        unsafe {
            std::env::set_var(
                "ROOM_TOKEN_SECRET",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &current),
            );
            std::env::set_var(
                "ROOM_TOKEN_SECRET_PREVIOUS",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &previous),
            );
        }
    });
}

fn build_request(path: &str, headers: &[(&str, &str)]) -> Request {
    let mut builder = http::Request::builder().method(Method::POST).uri(path);
    for (k, v) in headers {
        builder = builder.header(*k, HeaderValue::from_str(v).unwrap());
    }
    builder.body(Body::Empty).expect("valid request")
}

/// Assert a 401 carrying one of the two fixed messages. Matching the exact
/// strings is the point: the response must not reveal which check failed.
fn assert_unauthorized(err: Result<vercel_runtime::Response<Body>, vercel_runtime::Error>) {
    const FALLBACK: &str = "Your room session is invalid or expired. Please rejoin.";
    const MISSING: &str = "This action requires a room session.";

    let resp = err.expect("err variant should carry a built response");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "expected 401, got {}",
        resp.status()
    );
    let body = match resp.into_body() {
        Body::Text(s) => s,
        Body::Binary(b) => String::from_utf8(b).unwrap_or_default(),
        Body::Empty => String::new(),
    };
    let parsed: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("non-JSON body: {body}"));
    let msg = parsed
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        msg == FALLBACK || msg == MISSING,
        "auth body should be one of the two fixed messages, got: {msg}"
    );
}

fn current_secret() -> Vec<u8> {
    "A".repeat(32).into_bytes()
}

fn previous_secret() -> Vec<u8> {
    "B".repeat(32).into_bytes()
}

#[test]
fn rejects_request_with_no_authorization_header() {
    ensure_secrets();
    let req = build_request("/api/rooms/ABCDEF/start", &[]);
    let ctx = Ctx::from_request(&req);
    let err = require_room_auth(&req, &ctx, "ABCDEF").unwrap_err();
    assert_unauthorized(err);
}

#[test]
fn rejects_authorization_header_without_bearer_scheme() {
    ensure_secrets();
    let req = build_request(
        "/api/rooms/ABCDEF/start",
        &[("Authorization", "Basic dXNlcjpwYXNz")],
    );
    let ctx = Ctx::from_request(&req);
    let err = require_room_auth(&req, &ctx, "ABCDEF").unwrap_err();
    assert_unauthorized(err);
}

#[test]
fn rejects_malformed_token() {
    ensure_secrets();
    let req = build_request(
        "/api/rooms/ABCDEF/start",
        &[("Authorization", "Bearer not-a-real-token")],
    );
    let ctx = Ctx::from_request(&req);
    let err = require_room_auth(&req, &ctx, "ABCDEF").unwrap_err();
    assert_unauthorized(err);
}

#[test]
fn rejects_token_signed_by_unknown_secret() {
    // A token signed by neither secret must not slip through the fallback.
    ensure_secrets();
    let bogus = b"CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
    let token = auth::mint(bogus, "ABCDEF", "dev-1", chrono::Utc::now().timestamp()).expect("mint");
    let req = build_request(
        "/api/rooms/ABCDEF/start",
        &[("Authorization", &format!("Bearer {token}"))],
    );
    let ctx = Ctx::from_request(&req);
    let err = require_room_auth(&req, &ctx, "ABCDEF").unwrap_err();
    assert_unauthorized(err);
}

#[test]
fn rejects_token_for_a_different_room_code() {
    // The path's room code wins, so a token for another room unlocks
    // nothing.
    ensure_secrets();
    let token = auth::mint(
        &current_secret(),
        "ZZZZZZ",
        "dev-1",
        chrono::Utc::now().timestamp(),
    )
    .expect("mint");
    let req = build_request(
        "/api/rooms/ABCDEF/start",
        &[("Authorization", &format!("Bearer {token}"))],
    );
    let ctx = Ctx::from_request(&req);
    let err = require_room_auth(&req, &ctx, "ABCDEF").unwrap_err();
    assert_unauthorized(err);
}

#[test]
fn rejects_expired_token_on_regular_require_room_auth() {
    ensure_secrets();
    let long_ago = chrono::Utc::now().timestamp() - TOKEN_TTL_SECONDS - 60;
    let token = auth::mint(&current_secret(), "ABCDEF", "dev-1", long_ago).expect("mint");
    let req = build_request(
        "/api/rooms/ABCDEF/leave",
        &[("Authorization", &format!("Bearer {token}"))],
    );
    let ctx = Ctx::from_request(&req);
    let err = require_room_auth(&req, &ctx, "ABCDEF").unwrap_err();
    assert_unauthorized(err);
}

#[test]
fn accepts_expired_token_on_allow_expired_path() {
    // `/rejoin` accepts an expired token so the client can refresh. The
    // other checks still run; see the next test.
    ensure_secrets();
    let long_ago = chrono::Utc::now().timestamp() - TOKEN_TTL_SECONDS - 60;
    let token = auth::mint(&current_secret(), "ABCDEF", "dev-1", long_ago).expect("mint");
    let req = build_request(
        "/api/rooms/ABCDEF/rejoin",
        &[("Authorization", &format!("Bearer {token}"))],
    );
    let ctx = Ctx::from_request(&req);
    let claims = require_room_auth_allow_expired(&req, &ctx, "ABCDEF").expect("ok");
    assert_eq!(claims.did, "dev-1");
    assert_eq!(claims.rc, "ABCDEF");
}

#[test]
fn allow_expired_still_rejects_wrong_room_code() {
    // Otherwise any token holder could mint a credential for any room whose
    // code they knew.
    ensure_secrets();
    let token = auth::mint(
        &current_secret(),
        "ZZZZZZ",
        "dev-1",
        chrono::Utc::now().timestamp(),
    )
    .expect("mint");
    let req = build_request(
        "/api/rooms/ABCDEF/rejoin",
        &[("Authorization", &format!("Bearer {token}"))],
    );
    let ctx = Ctx::from_request(&req);
    let err = require_room_auth_allow_expired(&req, &ctx, "ABCDEF").unwrap_err();
    assert_unauthorized(err);
}

#[test]
fn allow_expired_still_rejects_bad_signature() {
    ensure_secrets();
    let bogus = b"DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";
    let token = auth::mint(bogus, "ABCDEF", "dev-1", chrono::Utc::now().timestamp()).expect("mint");
    let req = build_request(
        "/api/rooms/ABCDEF/rejoin",
        &[("Authorization", &format!("Bearer {token}"))],
    );
    let ctx = Ctx::from_request(&req);
    let err = require_room_auth_allow_expired(&req, &ctx, "ABCDEF").unwrap_err();
    assert_unauthorized(err);
}

#[test]
fn accepts_token_signed_by_previous_secret_during_rotation() {
    // A token minted under the previous secret must keep verifying until it
    // expires on its own.
    ensure_secrets();
    let token = auth::mint(
        &previous_secret(),
        "ABCDEF",
        "dev-old",
        chrono::Utc::now().timestamp(),
    )
    .expect("mint");
    let req = build_request(
        "/api/rooms/ABCDEF/advance",
        &[("Authorization", &format!("Bearer {token}"))],
    );
    let ctx = Ctx::from_request(&req);
    let claims = require_room_auth(&req, &ctx, "ABCDEF").expect("previous secret accepted");
    assert_eq!(claims.did, "dev-old");
}

#[test]
fn happy_path_returns_parsed_claims() {
    ensure_secrets();
    let token = auth::mint(
        &current_secret(),
        "ABCDEF",
        "dev-host",
        chrono::Utc::now().timestamp(),
    )
    .expect("mint");
    let req = build_request(
        "/api/rooms/ABCDEF/advance",
        &[("Authorization", &format!("Bearer {token}"))],
    );
    let ctx = Ctx::from_request(&req);
    let claims = require_room_auth(&req, &ctx, "ABCDEF").expect("ok");
    assert_eq!(claims.did, "dev-host");
    assert_eq!(claims.rc, "ABCDEF");
}

#[test]
fn request_id_round_trips_when_supplied() {
    // Inbound ids are echoed up to a length cap; anything missing, empty or
    // oversized gets a fresh UUID.
    ensure_secrets();
    let req = build_request(
        "/api/rooms/ABCDEF/start",
        &[(REQUEST_ID_HEADER, "req-from-edge-123")],
    );
    let ctx = Ctx::from_request(&req);
    assert_eq!(ctx.request_id, "req-from-edge-123");
}

#[test]
fn oversize_request_id_is_replaced_with_fresh_uuid() {
    ensure_secrets();
    let oversize = "x".repeat(500);
    let req = build_request("/api/rooms/ABCDEF/start", &[(REQUEST_ID_HEADER, &oversize)]);
    let ctx = Ctx::from_request(&req);
    assert_ne!(ctx.request_id, oversize);
    assert_eq!(ctx.request_id.len(), 36);
}

#[test]
fn extract_room_code_lowercase_path_uppercases() {
    // Every handler goes through this helper, so it must fold case.
    assert_eq!(
        vercel::extract_room_code("/api/rooms/abcdef/vote"),
        Some("ABCDEF".to_string())
    );
}
