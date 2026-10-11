//! POST /api/rooms/{code}/advance - host moves the group to the next pub.
//!
//! Needs a valid token. [`rooms::advance_room`] checks `claims.did` against
//! the room's host.

use pubroulette::rate_limit;
use pubroulette::rooms;
use pubroulette::vercel::{
    Ctx, enforce_rate_limit, error_response_room, extract_room_code, handle_options, json_error,
    json_response, require_room_auth,
};
use tracing::Instrument;
use vercel_runtime::{Body, Error, Request, Response as VercelResponse, StatusCode, run};

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(handler).await
}

async fn handler(req: Request) -> Result<VercelResponse<Body>, Error> {
    pubroulette::init_tracing();
    let mut ctx = Ctx::from_request(&req);
    let span = pubroulette::request_span("rooms_advance", &ctx.request_id);

    async move {
        if let Some(resp) = handle_options(&req) {
            return resp;
        }

        let code = match extract_room_code(req.uri().path()) {
            Some(c) => c,
            None => return json_error(&ctx, StatusCode::BAD_REQUEST, "Missing room code"),
        };

        let claims = match require_room_auth(&req, &ctx, &code) {
            Ok(c) => c,
            Err(resp) => return resp,
        };

        let mut conn = match rooms::connect().await {
            Ok(c) => c,
            Err(e) => return error_response_room(&ctx, &e),
        };

        if let Err(resp) = enforce_rate_limit(&mut conn, rate_limit::ADVANCE, &req, &mut ctx).await
        {
            return resp;
        }

        match rooms::advance_room(&mut conn, &code, &claims.did).await {
            Ok(room) => json_response(&ctx, StatusCode::OK, &room),
            Err(e) => error_response_room(&ctx, &e),
        }
    }
    .instrument(span)
    .await
}
