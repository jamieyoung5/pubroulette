//! Integration tests for the rate limiter, against a real Redis.
//!
//! `#[ignore]`d like the other Redis suites. Run with:
//!
//! ```sh
//! make redis-up
//! cargo test --test rate_limit_integration -- --ignored
//! ```

use pubroulette::rate_limit::{self, Limit, RateLimitError};
use redis::AsyncCommands;
use std::sync::Once;
use uuid::Uuid;

static SETUP: Once = Once::new();

fn setup() {
    SETUP.call_once(|| {
        let _ = dotenvy::dotenv();
    });
}

/// Generate a unique key per test so concurrent runs don't collide.
fn unique_key() -> String {
    Uuid::new_v4().to_string()
}

/// A fresh connection per test. `rooms::connect()` caches one, and its I/O
/// task is pinned to whichever test runtime opened it first.
async fn test_connect() -> redis::aio::MultiplexedConnection {
    setup();
    let url = std::env::var("REDIS_URL").expect("REDIS_URL must be set for integration tests");
    let client = redis::Client::open(url).expect("redis client");
    client
        .get_multiplexed_async_connection()
        .await
        .expect("connect to Redis")
}

async fn cleanup(bucket: &str, key: &str) {
    let mut conn = test_connect().await;
    let _: redis::RedisResult<()> = conn.del(format!("rate:{bucket}:{key}")).await;
}

#[tokio::test]
#[ignore]
async fn allows_traffic_under_limit() {
    setup();
    let mut conn = test_connect().await;
    let key = unique_key();
    let limit = Limit {
        bucket: "test_under",
        per_window: 5,
    };

    for _ in 0..5 {
        let state = rate_limit::check(&mut conn, limit, &key)
            .await
            .expect("under limit should pass");
        assert!(state.remaining <= limit.per_window);
    }

    cleanup(limit.bucket, &key).await;
}

#[tokio::test]
#[ignore]
async fn rejects_traffic_over_limit() {
    setup();
    let mut conn = test_connect().await;
    let key = unique_key();
    let limit = Limit {
        bucket: "test_over",
        per_window: 3,
    };

    for _ in 0..3 {
        rate_limit::check(&mut conn, limit, &key).await.expect("ok");
    }
    let err = rate_limit::check(&mut conn, limit, &key).await.unwrap_err();
    match err {
        RateLimitError::Exceeded { retry_after, state } => {
            assert!(retry_after > 0, "retry_after should be positive");
            assert!(retry_after <= 60, "retry_after should be <= window");
            assert_eq!(state.limit, limit.per_window);
            assert_eq!(state.remaining, 0, "no budget left after exceeding");
        }
        other => panic!("expected Exceeded, got {other:?}"),
    }

    cleanup(limit.bucket, &key).await;
}

#[tokio::test]
#[ignore]
async fn buckets_are_independent() {
    setup();
    let mut conn = test_connect().await;
    let key = unique_key();
    let bucket_a = Limit {
        bucket: "test_indep_a",
        per_window: 1,
    };
    let bucket_b = Limit {
        bucket: "test_indep_b",
        per_window: 1,
    };

    rate_limit::check(&mut conn, bucket_a, &key)
        .await
        .expect("a ok");
    let a_err = rate_limit::check(&mut conn, bucket_a, &key).await;
    assert!(matches!(a_err, Err(RateLimitError::Exceeded { .. })));

    rate_limit::check(&mut conn, bucket_b, &key)
        .await
        .expect("b should be independent");

    cleanup(bucket_a.bucket, &key).await;
    cleanup(bucket_b.bucket, &key).await;
}

#[tokio::test]
#[ignore]
async fn keys_are_independent() {
    setup();
    let mut conn = test_connect().await;
    let key_a = unique_key();
    let key_b = unique_key();
    let limit = Limit {
        bucket: "test_keys",
        per_window: 1,
    };

    rate_limit::check(&mut conn, limit, &key_a)
        .await
        .expect("a ok");
    let a_err = rate_limit::check(&mut conn, limit, &key_a).await;
    assert!(matches!(a_err, Err(RateLimitError::Exceeded { .. })));

    rate_limit::check(&mut conn, limit, &key_b)
        .await
        .expect("b ok");

    cleanup(limit.bucket, &key_a).await;
    cleanup(limit.bucket, &key_b).await;
}

#[tokio::test]
#[ignore]
async fn truncates_oversized_keys() {
    setup();
    let mut conn = test_connect().await;
    // 4KB of junk: the limiter must not choke on it or store it whole.
    let huge_key = "x".repeat(4096);
    let limit = Limit {
        bucket: "test_huge",
        per_window: 2,
    };

    rate_limit::check(&mut conn, limit, &huge_key)
        .await
        .expect("huge key should still work");
    rate_limit::check(&mut conn, limit, &huge_key)
        .await
        .expect("ok");
    let err = rate_limit::check(&mut conn, limit, &huge_key)
        .await
        .unwrap_err();
    assert!(matches!(err, RateLimitError::Exceeded { .. }));

    let truncated: String = huge_key.chars().take(128).collect();
    cleanup(limit.bucket, &truncated).await;
}
