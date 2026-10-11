//! Integration tests for the places cache (`src/places_cache.rs`).
//!
//! These need a real Redis and a `wiremock`-mocked Geoapify, so they are
//! `#[ignore]`d to keep plain `cargo test` hermetic. Run with:
//!
//! ```sh
//! make redis-up
//! cargo test --test places_cache_integration -- --ignored
//! ```
//!
//! Each test picks a random origin over the South Pacific and deletes every
//! key covering it before and after, so tests sharing a Redis stay isolated.

use pubroulette::places::{GeoapifyConfig, Mode};
use pubroulette::places_cache::{
    _test_cell_centre, _test_keys_for, _test_place_key, get_places_in_radius_with_conn,
};
use pubroulette::roulette::{self, PlaceFilters};
use rand::Rng;
use redis::AsyncCommands;
use std::collections::HashSet;
use std::sync::Once;
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

static SETUP: Once = Once::new();

fn setup() {
    SETUP.call_once(|| {
        let _ = dotenvy::dotenv();
    });
}

const TEST_API_KEY: &str = "test-key-cache";

/// A random origin in an empty patch of the South Pacific, big enough that
/// two tests almost never land in the same hex.
fn test_origin() -> (f64, f64) {
    let mut rng = rand::thread_rng();
    let lat = rng.gen_range(-50.0..-10.0);
    let lon = rng.gen_range(-160.0..-130.0);
    (lat, lon)
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

/// Delete every key a query at this origin would touch, in both modes.
/// Runs at the start and end of each test.
async fn cleanup(conn: &mut redis::aio::MultiplexedConnection, lat: f64, lon: f64, radius: i16) {
    for mode in [Mode::Pubs, Mode::NonAlcohol] {
        let (ledger_keys, hex_keys) = _test_keys_for(lat, lon, radius, mode);

        // Read the hex sets before deleting them, to find the place blobs.
        let mut place_ids: HashSet<String> = HashSet::new();
        for hk in &hex_keys {
            let members: Vec<String> = conn.smembers(hk).await.unwrap_or_default();
            place_ids.extend(members);
        }
        let place_keys: Vec<String> = place_ids
            .iter()
            .map(|id| _test_place_key(mode, id))
            .collect();

        for k in ledger_keys.iter().chain(&hex_keys).chain(&place_keys) {
            let _: i64 = conn.del(k.as_str()).await.unwrap_or(0);
        }
    }
}

/// Build a Geoapify-shaped GeoJSON feature for a place. Place ids must be
/// unique per test: blob keys are not origin-scoped, so two tests sharing an
/// id would delete each other's blobs on cleanup.
fn feature(name: &str, place_id: &str, lon: f64, lat: f64) -> serde_json::Value {
    serde_json::json!({
        "type": "Feature",
        "properties": {
            "name": name,
            "place_id": place_id,
            "osm_id": 10_000,
            "osm_type": "node",
            "formatted": format!("{name}, Some Street"),
            "address_line1": name,
            "address_line2": "Some Street",
            "categories": ["catering.pub"],
        },
        "geometry": { "type": "Point", "coordinates": [lon, lat] }
    })
}

fn feature_collection(features: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({ "type": "FeatureCollection", "features": features })
}

async fn setup_mock() -> (MockServer, GeoapifyConfig) {
    let server = MockServer::start().await;
    let config = GeoapifyConfig::new(server.uri(), TEST_API_KEY);
    (server, config)
}

/// Rough northward offset. Good enough for placing pubs a few hundred metres
/// apart.
fn offset_north(lat: f64, lon: f64, meters: f64) -> (f64, f64) {
    (lat + meters / 111_111.0, lon)
}

/// Rough eastward offset, scaled by latitude.
fn offset_east(lat: f64, lon: f64, meters: f64) -> (f64, f64) {
    let m_per_deg = 111_111.0 * lat.to_radians().cos().abs().max(0.05);
    (lat, lon + meters / m_per_deg)
}

#[tokio::test]
#[ignore]
async fn cold_cache_fires_one_geoapify_and_populates_redis() {
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;

    let (p1_lat, p1_lon) = offset_north(lat, lon, 100.0);
    let (p2_lat, p2_lon) = offset_north(lat, lon, 200.0);
    let body = feature_collection(vec![
        feature("The Oak", "pid_oak", p1_lon, p1_lat),
        feature("The Lion", "pid_lion", p2_lon, p2_lat),
    ]);
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        // wiremock checks the count when the server drops.
        .expect(1)
        .mount(&server)
        .await;

    let result = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .expect("cold cache fetch should succeed");

    assert_eq!(result.len(), 2, "expected both seeded pubs to come back");

    let (ledger_keys, hex_keys) = _test_keys_for(lat, lon, radius, Mode::Pubs);
    let any_ledger_set: bool = {
        let mut any = false;
        for k in &ledger_keys {
            let exists: bool = conn.exists(k).await.unwrap_or(false);
            if exists {
                any = true;
                break;
            }
        }
        any
    };
    assert!(any_ledger_set, "ledger should be marked for fetched hexes");

    let mut found_place_ids: HashSet<String> = HashSet::new();
    for hk in &hex_keys {
        let members: Vec<String> = conn.smembers(hk).await.unwrap_or_default();
        for m in members {
            found_place_ids.insert(m);
        }
    }
    assert!(found_place_ids.contains("pid_oak"));
    assert!(found_place_ids.contains("pid_lion"));

    let blob_oak: Option<String> = conn
        .get(_test_place_key(Mode::Pubs, "pid_oak"))
        .await
        .unwrap_or(None);
    assert!(blob_oak.is_some(), "pid_oak blob should be persisted");

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn warm_cache_serves_without_hitting_geoapify() {
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;

    let (p_lat, p_lon) = offset_north(lat, lon, 150.0);
    let body = feature_collection(vec![feature("The Anchor", "pid_anchor", p_lon, p_lat)]);
    // One call allowed: a warm query hitting Geoapify would make it two.
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let cold = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    assert_eq!(cold.len(), 1);

    let warm = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    assert_eq!(warm.len(), 1, "warm cache must return the same pub");
    assert_eq!(warm[0].place_id.as_deref(), Some("pid_anchor"));

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn partial_cover_fires_one_call_for_missing_area() {
    // B's cover overlaps A's but reaches past it, and B's missing hexes must
    // cost one upstream call, not one per hex. A is snapped to a cell centre
    // and B put 1000m east, past the cell boundary, so B reliably has missing
    // hexes; a random origin near an edge can leave B in A's own cell.
    let (seed_lat, seed_lon) = test_origin();
    let (lat_a, lon_a) = _test_cell_centre(seed_lat, seed_lon);
    let radius: i16 = 500;
    let (lat_b, lon_b) = offset_east(lat_a, lon_a, 1000.0);

    let mut conn = test_connect().await;
    cleanup(&mut conn, lat_a, lon_a, radius).await;
    cleanup(&mut conn, lat_b, lon_b, radius).await;

    let (server, config) = setup_mock().await;

    let (pa_lat, pa_lon) = offset_north(lat_a, lon_a, 100.0);
    let (pb_lat, pb_lon) = offset_north(lat_b, lon_b, 100.0);
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![
                feature("Pub A", "pid_partial_a", pa_lon, pa_lat),
                feature("Pub B", "pid_partial_b", pb_lon, pb_lat),
            ])),
        )
        // Two in total. One call per missing hex would make it more.
        .expect(2)
        .mount(&server)
        .await;

    let _ = get_places_in_radius_with_conn(
        &config,
        lat_a,
        lon_a,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    let _ = get_places_in_radius_with_conn(
        &config,
        lat_b,
        lon_b,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();

    cleanup(&mut conn, lat_a, lon_a, radius).await;
    cleanup(&mut conn, lat_b, lon_b, radius).await;
}

#[tokio::test]
#[ignore]
async fn distance_meters_recomputed_against_query_origin() {
    // A blob carries no distance, so each query must compute its own. Both
    // origins sit in one cell, which keeps the second query a cache hit; a
    // random origin near an edge would push origin 2 into a new cell and
    // trigger a second upstream call.
    let (seed_lat, seed_lon) = test_origin();
    let (lat, lon) = _test_cell_centre(seed_lat, seed_lon);
    let radius: i16 = 800;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;

    // Both the pub and origin 2 stay inside the cell, which is ~460m to a
    // vertex.
    let (pub_lat, pub_lon) = offset_north(lat, lon, 200.0);
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![feature(
                "The Crown",
                "pid_crown",
                pub_lon,
                pub_lat,
            )])),
        )
        // Same cell means the same cover, so the second query is warm.
        .expect(1)
        .mount(&server)
        .await;

    let r1 = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    assert_eq!(r1.len(), 1);
    let d1 = r1[0]
        .distance_meters
        .expect("distance should be recomputed");

    let (lat2, lon2) = offset_north(lat, lon, -100.0);
    let r2 = get_places_in_radius_with_conn(
        &config,
        lat2,
        lon2,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    let crown = r2
        .iter()
        .find(|e| e.place_id.as_deref() == Some("pid_crown"))
        .expect("cached pub should appear from second origin");
    let d2 = crown
        .distance_meters
        .expect("distance should be recomputed");

    assert!(
        d2 > d1 + 50.0,
        "expected origin-2 distance ({d2}) to be meaningfully larger than origin-1 ({d1})"
    );

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn bearing_filter_applied_after_cache_hydration() {
    // Bearing is applied after hydration, never stored, so a north query
    // must drop the southern pub.
    let (lat, lon) = test_origin();
    let radius: i16 = 1000;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;

    let (n_lat, n_lon) = offset_north(lat, lon, 300.0);
    let (s_lat, s_lon) = offset_north(lat, lon, -300.0);
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![
                feature("North Pub", "pid_n", n_lon, n_lat),
                feature("South Pub", "pid_s", s_lon, s_lat),
            ])),
        )
        // Only the bearing=0 call should populate the cache.
        .expect(1)
        .mount(&server)
        .await;

    let r = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        Some(0.0),
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    let ids: HashSet<&str> = r.iter().filter_map(|e| e.place_id.as_deref()).collect();
    assert!(
        ids.contains("pid_n"),
        "north pub should pass the bearing filter"
    );
    assert!(
        !ids.contains("pid_s"),
        "south pub should be filtered out by bearing=0"
    );

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn empty_region_is_cached_no_refetch() {
    // An empty region must still mark the ledger, or a rural area refetches
    // on every poll.
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;

    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(feature_collection(vec![])))
        // A second call would mean the absence wasn't cached.
        .expect(1)
        .mount(&server)
        .await;

    let r1 = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    assert!(r1.is_empty());

    let r2 = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    assert!(r2.is_empty());

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn far_region_pubs_do_not_leak_into_local_query() {
    // B's cover doesn't overlap A's, so B must not see A's pubs.
    let (lat_a, _lon_a) = test_origin();
    let lat_b = lat_a + 5.0;
    let lon_b = -135.0;
    let lon_a = -155.0;
    let radius: i16 = 500;

    let mut conn = test_connect().await;
    cleanup(&mut conn, lat_a, lon_a, radius).await;
    cleanup(&mut conn, lat_b, lon_b, radius).await;

    let (server, config) = setup_mock().await;

    let (pa_lat, pa_lon) = offset_north(lat_a, lon_a, 100.0);
    let (pb_lat, pb_lon) = offset_north(lat_b, lon_b, 100.0);

    // One mock per origin, on the same server.
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .and(query_param(
            "filter",
            format!("circle:{lon_a},{lat_a},{radius}").as_str(),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![feature(
                "Pub A", "pid_a", pa_lon, pa_lat,
            )])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .and(query_param(
            "filter",
            format!("circle:{lon_b},{lat_b},{radius}").as_str(),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![feature(
                "Pub B", "pid_b", pb_lon, pb_lat,
            )])),
        )
        .mount(&server)
        .await;

    let r_a = get_places_in_radius_with_conn(
        &config,
        lat_a,
        lon_a,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    assert_eq!(r_a.len(), 1);
    assert_eq!(r_a[0].place_id.as_deref(), Some("pid_a"));

    let r_b = get_places_in_radius_with_conn(
        &config,
        lat_b,
        lon_b,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    let ids: HashSet<&str> = r_b.iter().filter_map(|e| e.place_id.as_deref()).collect();
    assert!(ids.contains("pid_b"), "B's query should see its own pub");
    assert!(
        !ids.contains("pid_a"),
        "B's query must not see A's far-away pub"
    );

    cleanup(&mut conn, lat_a, lon_a, radius).await;
    cleanup(&mut conn, lat_b, lon_b, radius).await;
}

#[tokio::test]
#[ignore]
async fn place_in_multiple_hex_sets_returned_once() {
    // A place_id in two hex sets must come back once.
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (ledger_keys, hex_keys) = _test_keys_for(lat, lon, radius, Mode::Pubs);
    assert!(
        hex_keys.len() >= 2,
        "test requires a cover of at least 2 hexes; got {}",
        hex_keys.len()
    );

    for lk in &ledger_keys {
        let _: () = redis::cmd("SET")
            .arg(lk)
            .arg("1")
            .arg("EX")
            .arg(3600)
            .query_async(&mut conn)
            .await
            .unwrap();
    }

    let (pub_lat, pub_lon) = offset_north(lat, lon, 100.0);
    let cached_blob = serde_json::json!({
        "place_id": "pid_dup",
        "name": "The Dup",
        "latitude": pub_lat,
        "longitude": pub_lon,
        "outdoor_seating": false,
    })
    .to_string();
    let _: () = conn
        .set(_test_place_key(Mode::Pubs, "pid_dup"), &cached_blob)
        .await
        .unwrap();
    let _: i64 = conn.sadd(&hex_keys[0], "pid_dup").await.unwrap();
    let _: i64 = conn.sadd(&hex_keys[1], "pid_dup").await.unwrap();

    let (server, config) = setup_mock().await;
    // No mocks: falling through to Geoapify would fail the request.
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(feature_collection(vec![])))
        .mount(&server)
        .await;

    let r = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    let count = r
        .iter()
        .filter(|e| e.place_id.as_deref() == Some("pid_dup"))
        .count();
    assert_eq!(
        count, 1,
        "duplicate place_id across hex sets must collapse to one result"
    );

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn places_outside_radius_filtered_out() {
    // The cover overshoots the circle, so a pub inside the cover but outside
    // the radius must be dropped.
    let (lat, lon) = test_origin();
    let radius: i16 = 300;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (in_lat, in_lon) = offset_north(lat, lon, 200.0);
    let (out_lat, out_lon) = offset_north(lat, lon, 700.0);

    let (server, config) = setup_mock().await;
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![
                feature("In Pub", "pid_in", in_lon, in_lat),
                feature("Out Pub", "pid_out", out_lon, out_lat),
            ])),
        )
        .mount(&server)
        .await;

    let r = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .unwrap();
    let ids: HashSet<&str> = r.iter().filter_map(|e| e.place_id.as_deref()).collect();
    assert!(ids.contains("pid_in"), "in-radius pub must be returned");
    assert!(
        !ids.contains("pid_out"),
        "out-of-radius pub must be trimmed even if its hex was in the cover"
    );

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn ghost_member_in_hex_set_is_tolerated() {
    // A place_id whose blob was evicted must be skipped, not fatal.
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (ledger_keys, hex_keys) = _test_keys_for(lat, lon, radius, Mode::Pubs);
    for lk in &ledger_keys {
        let _: () = redis::cmd("SET")
            .arg(lk)
            .arg("1")
            .arg("EX")
            .arg(3600)
            .query_async(&mut conn)
            .await
            .unwrap();
    }

    let (real_lat, real_lon) = offset_north(lat, lon, 100.0);
    let real_blob = serde_json::json!({
        "place_id": "pid_real",
        "name": "Real Pub",
        "latitude": real_lat,
        "longitude": real_lon,
        "outdoor_seating": false,
    })
    .to_string();
    let _: () = conn
        .set(_test_place_key(Mode::Pubs, "pid_real"), &real_blob)
        .await
        .unwrap();
    let _: i64 = conn.sadd(&hex_keys[0], "pid_real").await.unwrap();

    let _: i64 = conn.sadd(&hex_keys[0], "pid_ghost").await.unwrap();

    let (server, config) = setup_mock().await;
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(feature_collection(vec![])))
        .mount(&server)
        .await;

    let r = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .expect("ghost member must not blow the call up");
    let ids: HashSet<&str> = r.iter().filter_map(|e| e.place_id.as_deref()).collect();
    assert!(ids.contains("pid_real"));
    assert!(!ids.contains("pid_ghost"));

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn redis_down_falls_back_to_direct_geoapify() {
    // `None` stands in for a failed connect: the query must still succeed
    // against Geoapify and cache nothing.
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;

    let (p_lat, p_lon) = offset_north(lat, lon, 150.0);
    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![feature(
                "The Fallback",
                "pid_fb",
                p_lon,
                p_lat,
            )])),
        )
        // One call per request, since nothing is cached.
        .expect(2)
        .mount(&server)
        .await;

    let r1 = get_places_in_radius_with_conn(&config, lat, lon, radius, None, Mode::Pubs, None)
        .await
        .expect("fallback should still succeed");
    let r2 = get_places_in_radius_with_conn(&config, lat, lon, radius, None, Mode::Pubs, None)
        .await
        .expect("fallback should still succeed on second call");

    assert_eq!(r1.len(), 1);
    assert_eq!(r2.len(), 1);

    let (ledger_keys, _hex_keys) = _test_keys_for(lat, lon, radius, Mode::Pubs);
    for lk in &ledger_keys {
        let exists: bool = conn.exists(lk).await.unwrap_or(false);
        assert!(
            !exists,
            "ledger key {lk} should not exist when Redis was bypassed"
        );
    }

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn roulette_routes_both_modes_through_the_cache() {
    // A second identical crawl must hit zero Geoapify, in either mode.
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;
    let (p_lat, p_lon) = offset_north(lat, lon, 100.0);

    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .and(query_param("categories", Mode::Pubs.categories()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![feature(
                "Pubs Pub", "pid_pubs", p_lon, p_lat,
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .and(query_param("categories", Mode::NonAlcohol.categories()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![feature(
                "Cafe", "pid_cafe", p_lon, p_lat,
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let filters = PlaceFilters::default();
    for mode in [Mode::Pubs, Mode::NonAlcohol] {
        for attempt in 0..2 {
            roulette::get_random_crawl(&config, 1, lat, lon, radius, None, mode, &[], &filters)
                .await
                .unwrap_or_else(|e| panic!("{mode} crawl {attempt} failed: {e:?}"));
        }
    }

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn modes_do_not_share_cached_places() {
    // A warm pubs cache must not answer a cafe query, and vice versa.
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;
    let (p_lat, p_lon) = offset_north(lat, lon, 100.0);

    Mock::given(method("GET"))
        .and(query_param("categories", Mode::Pubs.categories()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![feature(
                "The Oak",
                "pid_modes_oak",
                p_lon,
                p_lat,
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(query_param("categories", Mode::NonAlcohol.categories()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![
                feature("Bean There", "pid_bean", p_lon, p_lat),
                feature("Flat White", "pid_flat", p_lon, p_lat),
            ])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let pubs = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .expect("pubs query");
    let cafes = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::NonAlcohol,
        Some(conn.clone()),
    )
    .await
    .expect("cafe query");

    let pub_ids: HashSet<String> = pubs.iter().filter_map(|e| e.place_id.clone()).collect();
    let cafe_ids: HashSet<String> = cafes.iter().filter_map(|e| e.place_id.clone()).collect();

    assert_eq!(pub_ids, HashSet::from(["pid_modes_oak".to_string()]));
    assert_eq!(
        cafe_ids,
        HashSet::from(["pid_bean".to_string(), "pid_flat".to_string()])
    );

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn refetch_drops_places_that_disappeared_upstream() {
    // Ledger expiry forces a refetch. A place that is gone from the new
    // response must not linger in the hex set.
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;
    let (p_lat, p_lon) = offset_north(lat, lon, 100.0);

    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![
                feature("The Oak", "pid_refetch_oak", p_lon, p_lat),
                feature("The Closed Arms", "pid_closed", p_lon, p_lat),
            ])),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    let first = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .expect("cold query");
    assert_eq!(first.len(), 2);

    // Expire the ledger; the hex sets and blobs stay behind.
    let (ledger_keys, _) = _test_keys_for(lat, lon, radius, Mode::Pubs);
    for lk in &ledger_keys {
        let _: i64 = conn.del(lk.as_str()).await.unwrap_or(0);
    }

    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![feature(
                "The Oak",
                "pid_refetch_oak",
                p_lon,
                p_lat,
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let second = get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .expect("refetch query");

    let ids: HashSet<String> = second.iter().filter_map(|e| e.place_id.clone()).collect();
    assert_eq!(
        ids,
        HashSet::from(["pid_refetch_oak".to_string()]),
        "closed pub should be pruned on refetch"
    );

    cleanup(&mut conn, lat, lon, radius).await;
}

#[tokio::test]
#[ignore]
async fn cached_data_carries_a_ttl() {
    // Without TTLs on the data keys, abandoned regions would never evict.
    let (lat, lon) = test_origin();
    let radius: i16 = 500;
    let mut conn = test_connect().await;
    cleanup(&mut conn, lat, lon, radius).await;

    let (server, config) = setup_mock().await;
    let (p_lat, p_lon) = offset_north(lat, lon, 100.0);

    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(feature_collection(vec![feature(
                "The Oak",
                "pid_ttl_oak",
                p_lon,
                p_lat,
            )])),
        )
        .mount(&server)
        .await;

    get_places_in_radius_with_conn(
        &config,
        lat,
        lon,
        radius,
        None,
        Mode::Pubs,
        Some(conn.clone()),
    )
    .await
    .expect("cold query");

    let place_ttl: i64 = conn
        .ttl(_test_place_key(Mode::Pubs, "pid_ttl_oak"))
        .await
        .expect("place ttl");
    assert!(place_ttl > 0, "place blob should expire eventually");

    let (ledger_keys, hex_keys) = _test_keys_for(lat, lon, radius, Mode::Pubs);
    let mut checked_hex = false;
    for hk in &hex_keys {
        let members: i64 = conn.scard(hk.as_str()).await.unwrap_or(0);
        if members > 0 {
            let ttl: i64 = conn.ttl(hk.as_str()).await.expect("hex ttl");
            assert!(ttl > 0, "hex set {hk} should expire eventually");
            checked_hex = true;
        }
    }
    assert!(checked_hex, "expected at least one populated hex set");

    // The ledger must expire first, so a live ledger never points at gone data.
    let mut checked_ledger = false;
    for lk in &ledger_keys {
        let ttl: i64 = conn.ttl(lk.as_str()).await.unwrap_or(-2);
        if ttl > 0 {
            assert!(ttl < place_ttl, "ledger must expire before its data");
            checked_ledger = true;
        }
    }
    assert!(checked_ledger, "expected at least one ledger entry");

    cleanup(&mut conn, lat, lon, radius).await;
}
