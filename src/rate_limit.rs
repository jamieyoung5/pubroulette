//! Per-IP rate limiting backed by Redis.
//!
//! Each call increments `rate:{bucket}:{key}`, where `bucket` is the action and
//! `key` is the client IP. Over the bucket's limit returns
//! [`RateLimitError::Exceeded`] and the handler replies 429.
//!
//! Increment, first-hit TTL and state read happen in one Lua script, so two
//! concurrent requests can't both see `count < limit` and both pass. Redis
//! rather than an in-process limiter because every function is a fresh
//! process.

use redis::{AsyncCommands, Script};
use std::sync::LazyLock;
use tracing::warn;

/// Window length. Short enough that a tripped limit clears before a real user
/// gets frustrated.
const WINDOW_SECONDS: u64 = 60;

/// Atomic INCR + EXPIRE-on-first-hit + TTL fetch, returning `[count, ttl]`.
/// Cached as a [`Script`] so repeat calls use `EVALSHA`.
static LIMIT_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r#"
        local n = redis.call('INCR', KEYS[1])
        if n == 1 then
            redis.call('EXPIRE', KEYS[1], ARGV[1])
            return {n, tonumber(ARGV[1])}
        end
        local ttl = redis.call('TTL', KEYS[1])
        -- A key with no TTL (e.g. set by something else) gets one applied so
        -- it can't live forever and permanently block a client.
        if ttl < 0 then
            redis.call('EXPIRE', KEYS[1], ARGV[1])
            ttl = tonumber(ARGV[1])
        end
        return {n, ttl}
        "#,
    )
});

#[derive(Debug, Clone, Copy)]
pub struct Limit {
    pub bucket: &'static str,
    /// Max requests allowed in the `WINDOW_SECONDS` window.
    pub per_window: u32,
}

/// Bucket state after this request was counted. Rides out on response headers
/// so the client can show its own quota.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitState {
    pub limit: u32,
    /// Remaining budget. Stays at zero once over the limit.
    pub remaining: u32,
    /// Seconds until reset. Always >= 1 so a countdown always has a value.
    pub reset_seconds: u64,
}

/// Per-action limits. Generous for real UI use, tight for scripted abuse.
pub const CREATE: Limit = Limit {
    bucket: "create",
    per_window: 5,
};
pub const JOIN: Limit = Limit {
    bucket: "join",
    per_window: 10,
};
pub const START: Limit = Limit {
    bucket: "start",
    per_window: 10,
};
pub const ADVANCE: Limit = Limit {
    bucket: "advance",
    per_window: 30,
};
/// The busiest member write: clients resubmit as users toggle candidates.
/// Covers about one toggle per second.
pub const VOTE: Limit = Limit {
    bucket: "vote",
    per_window: 60,
};
/// Host-only and already capped per crawl by `MAX_SKIPS_PER_CRAWL`.
pub const SKIP: Limit = Limit {
    bucket: "skip",
    per_window: 10,
};
pub const LEAVE: Limit = Limit {
    bucket: "leave",
    per_window: 10,
};
/// The polling endpoint, so the most generous: ~1/s against a 3-5s client
/// poll interval.
pub const GET: Limit = Limit {
    bucket: "get",
    per_window: 60,
};
/// Single-pub roll. Each call can hit Geoapify, so this is the real per-IP
/// spend ceiling; the client-side daily counter can be cleared by anyone.
pub const PLAY: Limit = Limit {
    bucket: "play",
    per_window: 20,
};
/// Crawls cost more upstream than single rolls, so tighter than `PLAY`.
pub const CRAWL: Limit = Limit {
    bucket: "crawl",
    per_window: 10,
};
/// Nominatim proxy. Capped at a comfortable typing speed because their usage
/// policy forbids hammering.
pub const SEARCH: Limit = Limit {
    bucket: "search",
    per_window: 30,
};

#[derive(Debug)]
pub enum RateLimitError {
    Exceeded {
        retry_after: u64,
        state: RateLimitState,
    },
    /// Redis failed. Callers soft-fail: serving the request beats 500ing
    /// because the limiter is down.
    Storage,
}

/// Count this request and return the new state if it's under the limit.
/// `key` is a stable per-client identifier, normally an IP.
pub async fn check(
    conn: &mut redis::aio::MultiplexedConnection,
    limit: Limit,
    key: &str,
) -> Result<RateLimitState, RateLimitError> {
    // A huge X-Forwarded-For value must not become a huge Redis key.
    let safe_key: String = key.chars().take(128).collect();
    let redis_key = format!("rate:{}:{}", limit.bucket, safe_key);

    let result: Result<(i64, i64), _> = LIMIT_SCRIPT
        .key(&redis_key)
        .arg(WINDOW_SECONDS)
        .invoke_async(conn)
        .await;

    let (count, ttl) = match result {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, bucket = limit.bucket, "rate_limit_storage_error");
            return Err(RateLimitError::Storage);
        }
    };

    // At least 1s so `Retry-After` is always useful.
    let reset_seconds = ttl.max(1) as u64;
    let count_u32 = count.max(0) as u32;
    let remaining = limit.per_window.saturating_sub(count_u32);

    let state = RateLimitState {
        limit: limit.per_window,
        remaining,
        reset_seconds,
    };

    if count_u32 > limit.per_window {
        return Err(RateLimitError::Exceeded {
            retry_after: reset_seconds,
            state,
        });
    }

    Ok(state)
}

/// Read-only TTL probe for health checks and tests.
pub async fn peek_ttl(
    conn: &mut redis::aio::MultiplexedConnection,
    limit: Limit,
    key: &str,
) -> Option<i64> {
    let safe_key: String = key.chars().take(128).collect();
    let redis_key = format!("rate:{}:{}", limit.bucket, safe_key);
    conn.ttl(&redis_key).await.ok()
}

/// Best available client IP, in order: `x-vercel-forwarded-for`, `x-real-ip`,
/// then the *last* `x-forwarded-for` hop. The first two are edge-set and can't
/// be forged; the rightmost XFF hop is the only one a caller can't inject.
///
/// Falls back to `"unknown"`, which shares one bucket across un-IP'd traffic.
pub fn client_ip<'a, F>(get_header: F) -> String
where
    F: Fn(&str) -> Option<&'a str>,
{
    if let Some(v) = get_header("x-vercel-forwarded-for") {
        let trimmed = v.split(',').next().map(str::trim).unwrap_or("");
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(real) = get_header("x-real-ip") {
        let trimmed = real.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(xff) = get_header("x-forwarded-for") {
        // Rightmost hop is the edge-appended one.
        if let Some(last) = xff.split(',').next_back() {
            let trimmed = last.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn client_ip_prefers_vercel_header() {
        let mut h: HashMap<&str, &str> = HashMap::new();
        h.insert("x-vercel-forwarded-for", "203.0.113.1");
        // A forged XFF loses to the edge-set header.
        h.insert("x-forwarded-for", "1.2.3.4, 203.0.113.1");
        let ip = client_ip(|k| h.get(k).copied());
        assert_eq!(ip, "203.0.113.1");
    }

    #[test]
    fn client_ip_falls_back_to_real_ip() {
        let mut h: HashMap<&str, &str> = HashMap::new();
        h.insert("x-real-ip", "203.0.113.7");
        let ip = client_ip(|k| h.get(k).copied());
        assert_eq!(ip, "203.0.113.7");
    }

    #[test]
    fn client_ip_uses_xff_last_hop_when_no_vercel_or_real_ip() {
        // Reading the leftmost hop would let a caller pick their own bucket.
        let mut h: HashMap<&str, &str> = HashMap::new();
        h.insert("x-forwarded-for", "1.2.3.4, 10.0.0.1, 203.0.113.42");
        let ip = client_ip(|k| h.get(k).copied());
        assert_eq!(ip, "203.0.113.42");
    }

    #[test]
    fn client_ip_does_not_trust_xff_first_hop_against_vercel_header() {
        // A forged leading XFF entry must not win.
        let mut h: HashMap<&str, &str> = HashMap::new();
        h.insert("x-vercel-forwarded-for", "203.0.113.42");
        h.insert("x-forwarded-for", "1.2.3.4");
        let ip = client_ip(|k| h.get(k).copied());
        assert_eq!(ip, "203.0.113.42");
    }

    #[test]
    fn client_ip_unknown_when_no_headers() {
        let h: HashMap<&str, &str> = HashMap::new();
        let ip = client_ip(|k| h.get(k).copied());
        assert_eq!(ip, "unknown");
    }

    #[test]
    fn client_ip_handles_empty_xff() {
        let mut h: HashMap<&str, &str> = HashMap::new();
        h.insert("x-forwarded-for", "");
        h.insert("x-real-ip", "203.0.113.9");
        let ip = client_ip(|k| h.get(k).copied());
        assert_eq!(ip, "203.0.113.9");
    }
}
