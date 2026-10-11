//! POST /api/rooms/{code}/join - join a room, or refresh your name.
//!
//! Unauthenticated, because this is how a client gets its token. Rate-limited
//! per IP so it can't be used for spam.

use pubroulette::auth;
use pubroulette::rate_limit;
use pubroulette::rooms;
use pubroulette::vercel::{
    Ctx, enforce_rate_limit, error_response_auth, error_response_room, extract_room_code,
    handle_options, json_error, json_response, parse_body,
};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, info};
use vercel_runtime::{Body, Error, Request, Response as VercelResponse, StatusCode, run};

/// Join payload. The server mints `device_id`, so one on the body is ignored.
#[derive(Debug, Deserialize)]
struct JoinBody {
    name: String,
}

/// Same shape as create: room, token, and the `device_id` to persist for
/// later `/rejoin` calls.
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
    let span = pubroulette::request_span("rooms_join", &ctx.request_id);

    async move {
        if let Some(resp) = handle_options(&req) {
            return resp;
        }

        let code = match extract_room_code(req.uri().path()) {
            Some(c) => c,
            None => return json_error(&ctx, StatusCode::BAD_REQUEST, "Missing room code"),
        };

        let body = req.body().as_ref();
        let parsed: JoinBody = match parse_body(body) {
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

        // Minted here, so a device_id read from a poll response can't be
        // used to get a token bound to someone else. Clients persist the one
        // they're given and use `/rejoin` later.
        let device_id = uuid::Uuid::new_v4().to_string();

        info!(
            ip = %ctx.client_ip,
            device_id = %device_id,
            code = %code,
            "room_join_attempt"
        );

        match rooms::join_room(&mut conn, &code, device_id.clone(), parsed.name).await {
            Ok(room) => {
                let token = match auth::mint(
                    secret,
                    &room.code,
                    &device_id,
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
                        device_id: &device_id,
                    },
                )
            }
            Err(e) => error_response_room(&ctx, &e),
        }
    }
    .instrument(span)
    .await
}
