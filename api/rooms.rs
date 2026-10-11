//! POST /api/rooms - create a new room and return it with an auth token.

use pubroulette::auth;
use pubroulette::rate_limit;
use pubroulette::rooms::{self, RoomSettings};
use pubroulette::vercel::{
    Ctx, enforce_rate_limit, error_response_auth, error_response_room, handle_options, json_error,
    json_response, parse_body,
};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, info};
use vercel_runtime::{Body, Error, Request, Response as VercelResponse, StatusCode, run};

/// Create payload. The server mints `device_id` itself, so a `device_id` on
/// the body is ignored.
#[derive(Debug, Deserialize)]
struct CreateRoomBody {
    name: String,
    settings: RoomSettings,
}

/// Create and join both return the room, a fresh token, and the `device_id`
/// the client must persist.
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
    let span = pubroulette::request_span("rooms_create", &ctx.request_id);

    async move {
        if let Some(resp) = handle_options(&req) {
            return resp;
        }

        let body = req.body().as_ref();
        let parsed: CreateRoomBody = match parse_body(body) {
            Ok(p) => p,
            Err(msg) => return json_error(&ctx, StatusCode::BAD_REQUEST, &msg),
        };

        let mut conn = match rooms::connect().await {
            Ok(c) => c,
            Err(e) => return error_response_room(&ctx, &e),
        };

        if let Err(resp) = enforce_rate_limit(&mut conn, rate_limit::CREATE, &req, &mut ctx).await {
            return resp;
        }

        // Fail on missing config before writing a room we can't sign for.
        let secret = match auth::load_secret() {
            Ok(s) => s,
            Err(e) => return error_response_auth(&ctx, &e),
        };

        // Minted here, never read from the body: tokens are only ever issued
        // for ids the server produced, so a leaked device_id from a poll
        // response can't be claimed by someone else.
        let device_id = uuid::Uuid::new_v4().to_string();

        // Pairs the new device_id with the IP, so one IP creating many rooms
        // is visible.
        info!(
            ip = %ctx.client_ip,
            device_id = %device_id,
            "room_create_attempt"
        );

        match rooms::create_room(&mut conn, device_id.clone(), parsed.name, parsed.settings).await {
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
                    StatusCode::CREATED,
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
