//! GET /api/health - liveness and Redis reachability.
//!
//! 200 when Redis answers PING, 503 otherwise. No auth or rate limit, so
//! uptime monitors can poll it.

use pubroulette::rooms;
use pubroulette::vercel::{Ctx, handle_options, json_response};
use serde::Serialize;
use tracing::{Instrument, error, info};
use vercel_runtime::{Body, Error, Request, Response as VercelResponse, StatusCode, run};

#[derive(Debug, Serialize)]
struct HealthResponse<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    /// Baked in at build time, to confirm which build is live.
    version: &'a str,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(handler).await
}

async fn handler(req: Request) -> Result<VercelResponse<Body>, Error> {
    pubroulette::init_tracing();
    let ctx = Ctx::from_request(&req);
    let span = pubroulette::request_span("health", &ctx.request_id);

    async move {
        if let Some(resp) = handle_options(&req) {
            return resp;
        }

        let version = env!("CARGO_PKG_VERSION");

        let conn_result = rooms::connect().await;
        let mut conn = match conn_result {
            Ok(c) => c,
            Err(_e) => {
                error!("health_redis_connect_failed");
                return json_response(
                    &ctx,
                    StatusCode::SERVICE_UNAVAILABLE,
                    &HealthResponse {
                        ok: false,
                        reason: Some("redis_unreachable"),
                        version,
                    },
                );
            }
        };

        // Only the round-trip matters, not the payload.
        let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut conn).await;
        match pong {
            Ok(_) => {
                info!("health_ok");
                json_response(
                    &ctx,
                    StatusCode::OK,
                    &HealthResponse {
                        ok: true,
                        reason: None,
                        version,
                    },
                )
            }
            Err(e) => {
                error!(error = %e, "health_redis_ping_failed");
                json_response(
                    &ctx,
                    StatusCode::SERVICE_UNAVAILABLE,
                    &HealthResponse {
                        ok: false,
                        reason: Some("redis_ping_failed"),
                        version,
                    },
                )
            }
        }
    }
    .instrument(span)
    .await
}
