pub mod auth;
pub mod decision;
pub mod house_rules;
pub mod places;
pub mod places_cache;
pub mod rate_limit;
pub mod rooms;
pub mod roulette;
pub mod vercel;

use std::sync::Once;

static TRACING_INIT: Once = Once::new();

/// Install the JSON tracing subscriber, writing to stderr for the platform to
/// collect. `info` by default, overridable with `RUST_LOG`. Only the first
/// call does anything.
pub fn init_tracing() {
    TRACING_INIT.call_once(|| {
        use tracing_subscriber::{EnvFilter, fmt, prelude::*};

        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

        tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .json()
                    .with_target(true)
                    .with_span_list(true)
                    .without_time()
                    .with_writer(std::io::stderr),
            )
            .init();
    });
}

/// Top-level span for a request. Pass the id from [`vercel::Ctx::from_request`]
/// so the logs match the `X-Request-Id` the client was given.
pub fn request_span(endpoint: &str, request_id: &str) -> tracing::Span {
    tracing::info_span!(
        "request",
        request_id = %request_id,
        endpoint = %endpoint,
    )
}
