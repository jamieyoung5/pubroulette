//! Helpers shared by every function binary.
//!
//! Each endpoint in `api/` is its own binary, so without these the OPTIONS,
//! parse and error-mapping boilerplate would be copy-pasted per handler.

use crate::auth::{self, Claims, TokenError};
use crate::rate_limit::{self, Limit, RateLimitError, RateLimitState};
use crate::rooms::RoomError;
use http::Method;
use http::response::Builder as ResponseBuilder;
use serde::Serialize;
use tracing::{error, info, warn};
use vercel_runtime::{Body, Error, Request, Response as VercelResponse, StatusCode};

/// Per-request trace ID, logged as `request_id`.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Per-request metadata. Both fields ride out on every response, success or
/// error, so they travel together rather than as arguments everywhere.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Echoed in `X-Request-Id`. Inbound value if there was one, else a UUID.
    pub request_id: String,
    /// Set once so the limiter and the logs agree. See
    /// [`rate_limit::client_ip`].
    pub client_ip: String,
    /// Set by `enforce_rate_limit`. `None` means the limiter didn't run or
    /// soft-failed.
    pub rate_state: Option<RateLimitState>,
}

impl Ctx {
    /// Capture the request ID and client IP from an inbound request.
    pub fn from_request(req: &Request) -> Self {
        let client_ip =
            rate_limit::client_ip(|h| req.headers().get(h).and_then(|v| v.to_str().ok()));
        Self {
            request_id: extract_request_id(req),
            client_ip,
            rate_state: None,
        }
    }
}

fn extract_request_id(req: &Request) -> String {
    req.headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        // Cap inbound IDs so a hostile header can't bloat the logs.
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(|s| s.to_owned())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// `204` for CORS preflights. `Some(_)` when the caller should return early.
pub fn handle_options(req: &Request) -> Option<Result<VercelResponse<Body>, Error>> {
    if req.method() == Method::OPTIONS {
        let resp = VercelResponse::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::Empty)
            .map_err(|e| Error::from(e.to_string()));
        Some(resp)
    } else {
        None
    }
}

/// `204` with the request-id echo, for handlers with no success body.
pub fn no_content(ctx: &Ctx) -> Result<VercelResponse<Body>, Error> {
    let builder = VercelResponse::builder()
        .status(StatusCode::NO_CONTENT)
        .header(REQUEST_ID_HEADER, &ctx.request_id);
    let builder = with_rate_limit_headers(builder, ctx.rate_state);
    Ok(builder.body(Body::Empty)?)
}

pub fn json_response<T: Serialize>(
    ctx: &Ctx,
    status: StatusCode,
    body: &T,
) -> Result<VercelResponse<Body>, Error> {
    json_response_with_headers(ctx, status, body, &[])
}

/// [`json_response`] plus extra headers. The polling endpoint uses it for
/// `Cache-Control: no-store`.
pub fn json_response_with_headers<T: Serialize>(
    ctx: &Ctx,
    status: StatusCode,
    body: &T,
    extra_headers: &[(&str, &str)],
) -> Result<VercelResponse<Body>, Error> {
    let json = serde_json::to_string(body)?;
    let mut builder = VercelResponse::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header(REQUEST_ID_HEADER, &ctx.request_id);
    for (k, v) in extra_headers {
        builder = builder.header(*k, *v);
    }
    let builder = with_rate_limit_headers(builder, ctx.rate_state);
    Ok(builder.body(json.into())?)
}

pub fn json_error(
    ctx: &Ctx,
    status: StatusCode,
    message: &str,
) -> Result<VercelResponse<Body>, Error> {
    let body = serde_json::json!({ "error": message }).to_string();
    let builder = VercelResponse::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header(REQUEST_ID_HEADER, &ctx.request_id);
    let builder = with_rate_limit_headers(builder, ctx.rate_state);
    Ok(builder.body(body.into())?)
}

/// Turn a [`TokenError`] into a 401. Every variant collapses to one message so
/// the response never says which check failed.
pub fn error_response_auth(ctx: &Ctx, err: &TokenError) -> Result<VercelResponse<Body>, Error> {
    let status = match err {
        TokenError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::UNAUTHORIZED,
    };
    if status.is_server_error() {
        error!(error = ?err, "auth_config_error");
    } else {
        info!(error = ?err, "auth_invalid");
    }
    json_error(ctx, status, err.client_message())
}

/// Verify the `Authorization: Bearer` token against the expected room code.
/// The error is a finished 401 response, so handlers can return it directly.
#[allow(clippy::result_large_err)]
pub fn require_room_auth(
    req: &Request,
    ctx: &Ctx,
    expected_code: &str,
) -> Result<Claims, Result<VercelResponse<Body>, Error>> {
    let token = match extract_bearer_token(req) {
        Some(t) => t,
        None => return Err(error_response_auth(ctx, &TokenError::Missing)),
    };
    let now = chrono::Utc::now().timestamp();
    auth::verify_with_rotation(token, expected_code, now).map_err(|e| error_response_auth(ctx, &e))
}

/// [`require_room_auth`] but accepting expired tokens. Only `/rejoin` uses it:
/// a stale signed token proves prior membership. Every other check still runs.
#[allow(clippy::result_large_err)]
pub fn require_room_auth_allow_expired(
    req: &Request,
    ctx: &Ctx,
    expected_code: &str,
) -> Result<Claims, Result<VercelResponse<Body>, Error>> {
    let token = match extract_bearer_token(req) {
        Some(t) => t,
        None => return Err(error_response_auth(ctx, &TokenError::Missing)),
    };
    auth::verify_with_rotation_allow_expired(token, expected_code)
        .map_err(|e| error_response_auth(ctx, &e))
}

fn extract_bearer_token(req: &Request) -> Option<&str> {
    let value = req.headers().get(http::header::AUTHORIZATION)?;
    let s = value.to_str().ok()?;
    // Case-insensitive scheme, one or more spaces.
    let mut parts = s.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = parts.next()?.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// Apply the per-IP rate limit. Stores the result on `ctx` so later responses
/// pick up the `X-RateLimit-*` headers. Returns 429 when over the limit, and
/// soft-fails when Redis is down.
#[allow(clippy::result_large_err)]
pub async fn enforce_rate_limit(
    conn: &mut redis::aio::MultiplexedConnection,
    limit: Limit,
    _req: &Request,
    ctx: &mut Ctx,
) -> Result<(), Result<VercelResponse<Body>, Error>> {
    match rate_limit::check(conn, limit, &ctx.client_ip).await {
        Ok(state) => {
            ctx.rate_state = Some(state);
            Ok(())
        }
        Err(RateLimitError::Exceeded { retry_after, state }) => {
            info!(
                bucket = limit.bucket,
                retry_after, ip = %ctx.client_ip, "rate_limited"
            );
            // So a later json_error carries the same headers.
            ctx.rate_state = Some(state);
            Err(rate_limited_response(ctx, retry_after))
        }
        Err(RateLimitError::Storage) => Ok(()),
    }
}

/// Attach the `X-RateLimit-*` headers. No-op when `state` is `None`. The
/// deploy config must expose these for the browser to read them.
fn with_rate_limit_headers(
    mut builder: ResponseBuilder,
    state: Option<RateLimitState>,
) -> ResponseBuilder {
    if let Some(s) = state {
        builder = builder
            .header("X-RateLimit-Limit", s.limit.to_string())
            .header("X-RateLimit-Remaining", s.remaining.to_string())
            .header("X-RateLimit-Reset", s.reset_seconds.to_string());
    }
    builder
}

fn rate_limited_response(ctx: &Ctx, retry_after: u64) -> Result<VercelResponse<Body>, Error> {
    let body = serde_json::json!({
        "error": "Too many requests. Please slow down."
    })
    .to_string();
    let builder = VercelResponse::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("Content-Type", "application/json")
        .header("Retry-After", retry_after.to_string())
        .header(REQUEST_ID_HEADER, &ctx.request_id);
    let builder = with_rate_limit_headers(builder, ctx.rate_state);
    Ok(builder.body(body.into())?)
}

pub fn error_response_room(ctx: &Ctx, err: &RoomError) -> Result<VercelResponse<Body>, Error> {
    let (status, message) = err.to_response();
    // The client message is scrubbed for some variants, so this log is the
    // only place the original error survives.
    if status.is_server_error() {
        error!(status = status.as_u16(), error = %message, room_error = ?err, "response_error");
    } else {
        info!(status = status.as_u16(), error = %message, room_error = ?err, "response_client_error");
    }

    let body = serde_json::json!({ "error": message }).to_string();
    let mut builder = VercelResponse::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header(REQUEST_ID_HEADER, &ctx.request_id);
    builder = with_rate_limit_headers(builder, ctx.rate_state);
    if let Some(seconds) = err.retry_after_seconds() {
        builder = builder.header("Retry-After", seconds.to_string());
    }
    Ok(builder.body(body.into())?)
}

pub fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    serde_json::from_slice(body).map_err(|e| {
        warn!(
            error = %e,
            body_preview = %String::from_utf8_lossy(&body[..body.len().min(200)]),
            "request_parse_error"
        );
        format!("Invalid JSON: {e}")
    })
}

/// Pull `{code}` out of `/api/rooms/{code}` or `/api/rooms/{code}/<action>`.
/// Upper-cased, because codes are minted from an uppercase alphabet and a user
/// may type the link in lower case.
pub fn extract_room_code(path: &str) -> Option<String> {
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    let api = segments.next()?;
    let rooms = segments.next()?;
    if api != "api" || rooms != "rooms" {
        return None;
    }
    let code = segments.next()?;
    if code.is_empty() {
        return None;
    }
    Some(code.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_room_code_from_get_path() {
        assert_eq!(
            extract_room_code("/api/rooms/ABCDEF"),
            Some("ABCDEF".to_string())
        );
    }

    #[test]
    fn extract_room_code_from_action_path() {
        assert_eq!(
            extract_room_code("/api/rooms/ABCDEF/join"),
            Some("ABCDEF".to_string())
        );
        assert_eq!(
            extract_room_code("/api/rooms/ABCDEF/start"),
            Some("ABCDEF".to_string())
        );
    }

    #[test]
    fn extract_room_code_handles_trailing_slash() {
        assert_eq!(
            extract_room_code("/api/rooms/ABCDEF/"),
            Some("ABCDEF".to_string())
        );
    }

    #[test]
    fn extract_room_code_uppercases_lowercase_input() {
        // A lowercased deep link must reach the same room.
        assert_eq!(
            extract_room_code("/api/rooms/abcdef"),
            Some("ABCDEF".to_string())
        );
        assert_eq!(
            extract_room_code("/api/rooms/AbCdEf/join"),
            Some("ABCDEF".to_string())
        );
    }

    #[test]
    fn extract_room_code_rejects_other_paths() {
        assert_eq!(extract_room_code("/api/play"), None);
        assert_eq!(extract_room_code("/api/rooms"), None);
        assert_eq!(extract_room_code("/api/rooms/"), None);
        assert_eq!(extract_room_code("/"), None);
    }
}
