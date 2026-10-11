//! POST /api/rooms/{code}/rejoin - fresh token for an existing member, for the
//! client's silent refresh after a 401.
//!
//! Needs a token with a valid signature for this room, but ignores `exp`,
//! since an expired token is the reason to call. Membership is re-checked in
//! Lua from `claims.did`, never the body, so the room code alone gets you
//! nothing. First-time entry is `/join`, not this.

use pubroulette::auth;
use pubroulette::rate_limit;
use pubroulette::rooms;
use pubroulette::vercel::{
    Ctx, enforce_rate_limit, error_response_auth, error_response_room, extract_room_code,
    handle_options, json_error, json_response, parse_body, require_room_auth_allow_expired,
};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, info};
use vercel_runtime::{Body, Error, Request, Response as VercelResponse, StatusCode, run};

#[derive(Debug, Deserialize)]
struct RejoinBody {
    /// Display name, which may have changed in another tab. Rejoining
    /// refreshes it on the existing member record.
    name: String,
}

/// Same shape as create and join. `device_id` comes from `claims.did`, so a
/// client that lost its copy can recover it.
#[derive(Debug, Serialize)]
struct RoomWithToken<'a> {
    room: &'a rooms::Room,
    token: String,
    device_id: &'a str,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(handler).await
}

async fn handler(req: Request) -> Result<VercelResponse<Body>, Error> {
    pubroulette::init_tracing();
    let mut ctx = Ctx::from_request(&req);
    let span = pubroulette::request_span("rooms_rejoin", &ctx.request_id);

    async move {
        if let Some(resp) = handle_options(&req) {
            return resp;
        }

        let code = match extract_room_code(req.uri().path()) {
            Some(c) => c,
            None => return json_error(&ctx, StatusCode::BAD_REQUEST, "Missing room code"),
        };

        // Identity comes from the token, never the body. Expired is fine
        // here; a bad signature or room code still fails.
        let claims = match require_room_auth_allow_expired(&req, &ctx, &code) {
            Ok(c) => c,
            Err(resp) => return resp,
        };

        let body = req.body().as_ref();
        let parsed: RejoinBody = match parse_body(body) {
            Ok(p) => p,
            Err(msg) => return json_error(&ctx, StatusCode::BAD_REQUEST, &msg),
        };

        let mut conn = match rooms::connect().await {
            Ok(c) => c,
            Err(e) => return error_response_room(&ctx, &e),
        };

        if let Err(resp) = enforce_rate_limit(&mut conn, rate_limit::JOIN, &req, &mut ctx).await {
            return resp;
        }

        let secret = match auth::load_secret() {
            Ok(s) => s,
            Err(e) => return error_response_auth(&ctx, &e),
        };

        info!(
            ip = %ctx.client_ip,
            device_id = %claims.did,
            code = %code,
            "room_rejoin_attempt"
        );

        match rooms::rejoin_room(&mut conn, &code, claims.did.clone(), parsed.name).await {
            Ok(room) => {
                let token = match auth::mint(
                    secret,
                    &room.code,
                    &claims.did,
                    chrono::Utc::now().timestamp(),
                ) {
                    Ok(t) => t,
                    Err(e) => return error_response_auth(&ctx, &e),
                };
                json_response(
                    &ctx,
                    StatusCode::OK,
                    &RoomWithToken {
                        room: &room,
                        token,
                        device_id: &claims.did,
                    },
                )
            }
            Err(e) => error_response_room(&ctx, &e),
        }
    }
    .instrument(span)
    .await
}
