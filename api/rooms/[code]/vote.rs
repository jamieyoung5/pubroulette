//! POST /api/rooms/{code}/vote - approval vote on the current round.
//!
//! Needs a valid token, which only proves it was minted for this room.
//! [`rooms::vote_room`] checks that the caller is still a member.

use pubroulette::rate_limit;
use pubroulette::rooms;
use pubroulette::vercel::{
    Ctx, enforce_rate_limit, error_response_room, extract_room_code, handle_options, json_error,
    json_response, parse_body, require_room_auth,
};
use serde::Deserialize;
use tracing::Instrument;
use vercel_runtime::{Body, Error, Request, Response as VercelResponse, StatusCode, run};

#[derive(Debug, Deserialize)]
struct VoteBody {
    /// Approved candidates. An empty array clears the caller's vote.
    approvals: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(handler).await
}

async fn handler(req: Request) -> Result<VercelResponse<Body>, Error> {
    pubroulette::init_tracing();
    let mut ctx = Ctx::from_request(&req);
    let span = pubroulette::request_span("rooms_vote", &ctx.request_id);

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

        let body = req.body().as_ref();
        let parsed: VoteBody = match parse_body(body) {
            Ok(p) => p,
            Err(msg) => return json_error(&ctx, StatusCode::BAD_REQUEST, &msg),
        };

        let mut conn = match rooms::connect().await {
            Ok(c) => c,
            Err(e) => return error_response_room(&ctx, &e),
        };

        if let Err(resp) = enforce_rate_limit(&mut conn, rate_limit::VOTE, &req, &mut ctx).await {
            return resp;
        }

        match rooms::vote_room(&mut conn, &code, &claims.did, &parsed.approvals).await {
            Ok(room) => json_response(&ctx, StatusCode::OK, &room),
            Err(e) => error_response_room(&ctx, &e),
        }
    }
    .instrument(span)
    .await
}
