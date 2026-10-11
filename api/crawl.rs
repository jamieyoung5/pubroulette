use http::Method;
use pubroulette::places::{GeoapifyConfig, Mode};
use pubroulette::rate_limit;
use pubroulette::rooms;
use pubroulette::roulette::{AppError, PlaceFilters, get_random_crawl};
use pubroulette::vercel::{Ctx, enforce_rate_limit, json_error, json_response};
use serde::Deserialize;
use tracing::{Instrument, error, info, warn};
use validator::Validate;
use vercel_runtime::{
    Body, Error, Request, Response as VercelResponse, StatusCode as VercelStatusCode, run,
};

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

fn error_response(ctx: &Ctx, err: &AppError) -> Result<VercelResponse<Body>, Error> {
    let (status, message) = err.to_response();

    if status.is_server_error() {
        error!(status = status.as_u16(), error = %message, "response_error");
    } else {
        info!(status = status.as_u16(), error = %message, "response_client_error");
    }

    json_error(ctx, status, &message)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(handler).await
}

async fn handler(req: Request) -> Result<VercelResponse<Body>, Error> {
    pubroulette::init_tracing();
    let mut ctx = Ctx::from_request(&req);
    let span = pubroulette::request_span("crawl", &ctx.request_id);

    async move {
        if req.method() == Method::OPTIONS {
            return Ok(VercelResponse::builder()
                .status(VercelStatusCode::NO_CONTENT)
                .body(Body::Empty)?);
        }

        // Before any upstream call. Tighter bucket than single rolls.
        match rooms::connect().await {
            Ok(mut conn) => {
                if let Err(resp) =
                    enforce_rate_limit(&mut conn, rate_limit::CRAWL, &req, &mut ctx).await
                {
                    return resp;
                }
            }
            Err(e) => {
                warn!(error = ?e, "rate_limit_redis_unavailable");
            }
        }

        let body = req.body().as_ref();

        let params: CrawlParams = match serde_json::from_slice(body) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, body_preview = %String::from_utf8_lossy(&body[..body.len().min(200)]), "request_parse_error");
                return error_response(&ctx, &AppError::ParseError(e.to_string()));
            }
        };

        if let Err(e) = params.validate() {
            warn!(error = %e, latitude = params.latitude, longitude = params.longitude, radius = params.radius, num_pubs = params.num_pubs, "request_validation_error");
            return error_response(&ctx, &AppError::from(e));
        }

        let config = match GeoapifyConfig::from_env() {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "config_error");
                return error_response(&ctx, &AppError::OsmError(e));
            }
        };

        let filters = PlaceFilters {
            beer_garden: params.beer_garden,
        };

        match get_random_crawl(
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
        {
            Ok(pub_responses) => json_response(&ctx, VercelStatusCode::OK, &pub_responses),
            Err(e) => error_response(&ctx, &e),
        }
    }
    .instrument(span)
    .await
}
