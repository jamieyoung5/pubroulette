//! GET /api/rooms/{code} - poll room state.
//!
//! Unauthenticated, because sharing the code is how people join. The code
//! plus per-IP rate limiting is the brute-force defence, and without a token
//! this endpoint can only read.

use http::Method;
use pubroulette::rate_limit;
use pubroulette::rooms;
use pubroulette::vercel::{
    Ctx, enforce_rate_limit, error_response_room, extract_room_code, handle_options, json_error,
    json_response_with_headers,
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
    let span = pubroulette::request_span("rooms_get", &ctx.request_id);

    async move {
        if let Some(resp) = handle_options(&req) {
            return resp;
        }
        if req.method() != Method::GET {
            return json_error(&ctx, StatusCode::METHOD_NOT_ALLOWED, "Use GET");
        }

        let code = match extract_room_code(req.uri().path()) {
            Some(c) => c,
            None => return json_error(&ctx, StatusCode::BAD_REQUEST, "Missing room code"),
        };

        let mut conn = match rooms::connect().await {
            Ok(c) => c,
            Err(e) => return error_response_room(&ctx, &e),
        };

        if let Err(resp) = enforce_rate_limit(&mut conn, rate_limit::GET, &req, &mut ctx).await {
            return resp;
        }

        // Resolve an expired round here so the group isn't stuck when the
        // host goes offline. A no-op otherwise. Because this GET can write,
        // every response is `no-store`, or a cache would swallow the poll
        // that triggers it.
        let extra = &[("Cache-Control", "no-store")];
        match rooms::resolve_room_if_expired(&mut conn, &code).await {
            Ok(room) => json_response_with_headers(&ctx, StatusCode::OK, &room, extra),
            Err(e) => error_response_room(&ctx, &e),
        }
    }
    .instrument(span)
    .await
}
