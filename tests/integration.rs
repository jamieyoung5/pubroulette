use pubroulette::places::{self, GeoapifyConfig, Mode};
use pubroulette::roulette::{self, AppError, PlaceFilters, PubResponse};
use std::collections::HashSet;
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn feature(name: &str, place_id: &str, lon: f64, lat: f64) -> serde_json::Value {
    serde_json::json!({
        "type": "Feature",
        "properties": {
            "name": name,
            "place_id": place_id,
            "osm_id": 10000,
            "osm_type": "node",
            "formatted": format!("{name}, Some Street, Some City"),
            "address_line1": name,
            "address_line2": "Some Street, Some City",
            "categories": ["catering.pub"]
        },
        "geometry": {
            "type": "Point",
            "coordinates": [lon, lat]
        }
    })
}

fn feature_messy(
    name: &str,
    place_id: &str,
    lon: f64,
    lat: f64,
    housenumber: serde_json::Value,
    postcode: serde_json::Value,
    osm_id: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "type": "Feature",
        "properties": {
            "name": name,
            "place_id": place_id,
            "osm_id": osm_id,
            "osm_type": "way",
            "housenumber": housenumber,
            "postcode": postcode,
            "formatted": format!("{name}, Some Street"),
            "address_line1": name,
            "address_line2": "Some Street",
            "categories": ["catering.pub", "fee", "internet_access.free"],
            "distance": 42.5,
            "some_future_field": { "nested": true },
            "another_new_array": [1, 2, 3]
        },
        "geometry": {
            "type": "Point",
            "coordinates": [lon, lat]
        }
    })
}

fn feature_collection(features: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "type": "FeatureCollection",
        "features": features
    })
}

const TEST_API_KEY: &str = "test-key-12345";

static CACHE_OFF: std::sync::Once = std::sync::Once::new();

/// Keep these tests off any Redis the developer happens to have configured.
/// With a reachable cache, results would carry between tests and across runs.
fn disable_places_cache() {
    CACHE_OFF.call_once(|| {
        // SAFETY: process-global mutation, but `Once` runs it before any test
        // reaches the cache.
        unsafe { std::env::set_var("PLACES_CACHE_DISABLED", "1") };
    });
}

/// A mock server and a config pointing at it, so tests can run in parallel.
async fn setup_mock() -> (MockServer, GeoapifyConfig) {
    disable_places_cache();
    let server = MockServer::start().await;
    let config = GeoapifyConfig::new(server.uri(), TEST_API_KEY);
    (server, config)
}

#[tokio::test]
async fn test_get_elements_returns_parsed_elements() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("The Royal Oak", "id_1", -0.1, 51.5),
        feature("The Red Lion", "id_2", -0.11, 51.51),
        feature("The Crown", "id_3", -0.09, 51.49),
    ]);

    Mock::given(method("GET"))
        .and(query_param("apiKey", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs)
        .await
        .expect("should succeed");

    assert_eq!(elements.len(), 3);

    let names: HashSet<&str> = elements.iter().map(|e| e.names.name.as_str()).collect();
    assert!(names.contains("The Royal Oak"));
    assert!(names.contains("The Red Lion"));
    assert!(names.contains("The Crown"));

    let oak = elements
        .iter()
        .find(|e| e.names.name == "The Royal Oak")
        .unwrap();
    assert!((oak.location.latitude - 51.5).abs() < 1e-6);
    assert!((oak.location.longitude - (-0.1)).abs() < 1e-6);
    assert_eq!(oak.place_id.as_deref(), Some("id_1"));
    assert_eq!(oak.address_line1.as_deref(), Some("The Royal Oak"));
}

#[tokio::test]
async fn test_get_elements_empty_response() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs)
        .await
        .expect("should succeed even with zero features");

    assert!(elements.is_empty());
}

#[tokio::test]
async fn test_get_elements_upstream_500() {
    let (server, config) = setup_mock().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal server error"))
        .expect(1)
        .mount(&server)
        .await;

    let err = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs)
        .await
        .expect_err("should propagate upstream error");

    assert!(err.contains("non-success status"));
}

#[tokio::test]
async fn test_get_elements_upstream_401() {
    let (server, config) = setup_mock().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            serde_json::json!({"error": "Unauthorized", "message": "Invalid apiKey"}),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let err = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs)
        .await
        .expect_err("should fail on 401");

    assert!(err.contains("non-success status"));
}

#[tokio::test]
async fn test_get_elements_invalid_json_response() {
    let (server, config) = setup_mock().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not json"))
        .expect(1)
        .mount(&server)
        .await;

    let err = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs)
        .await
        .expect_err("should fail on invalid JSON");

    assert!(err.contains("expected") || err.contains("invalid"));
}

#[tokio::test]
async fn test_get_elements_integer_housenumber_does_not_break_parsing() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("Pub A", "id_a", -0.1, 51.5),
        feature_messy(
            "Pub B",
            "id_b",
            -0.11,
            51.51,
            serde_json::json!(4042),  // housenumber as int
            serde_json::json!(12345), // postcode as int
            serde_json::json!(99999), // osm_id as int (normal)
        ),
        feature_messy(
            "Pub C",
            "id_c",
            -0.09,
            51.49,
            serde_json::json!("17B"),   // housenumber as string
            serde_json::json!("SW1A"),  // postcode as string
            serde_json::json!("88888"), // osm_id as string
        ),
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs)
        .await
        .expect("integer housenumber must not break parsing");

    assert_eq!(elements.len(), 3);

    let pub_b = elements.iter().find(|e| e.names.name == "Pub B").unwrap();
    assert_eq!(pub_b.place_id.as_deref(), Some("id_b"));
    assert_eq!(pub_b.id, "99999");

    let pub_c = elements.iter().find(|e| e.names.name == "Pub C").unwrap();
    assert_eq!(pub_c.id, "88888");
}

#[tokio::test]
async fn test_get_elements_feature_with_no_name_is_skipped() {
    let (server, config) = setup_mock().await;

    let nameless = serde_json::json!({
        "type": "Feature",
        "properties": {
            "categories": ["catering.pub"]
        },
        "geometry": {
            "type": "Point",
            "coordinates": [-0.1, 51.5]
        }
    });

    let body = feature_collection(vec![nameless, feature("Named Pub", "id_named", -0.1, 51.5)]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs)
        .await
        .expect("should succeed");

    assert_eq!(elements.len(), 1);
    assert_eq!(elements[0].names.name, "Named Pub");
}

#[tokio::test]
async fn test_get_elements_name_falls_back_to_formatted() {
    let (server, config) = setup_mock().await;

    let no_name_but_formatted = serde_json::json!({
        "type": "Feature",
        "properties": {
            "formatted": "123 High Street, Edinburgh",
            "address_line1": "123 High Street",
            "place_id": "fallback_id",
            "osm_id": 1,
            "categories": ["catering.pub"]
        },
        "geometry": {
            "type": "Point",
            "coordinates": [-3.2, 55.9]
        }
    });

    let body = feature_collection(vec![no_name_but_formatted]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 55.9, -3.2, 500, None, Mode::Pubs)
        .await
        .expect("should succeed");

    assert_eq!(elements.len(), 1);
    assert_eq!(elements[0].names.name, "123 High Street, Edinburgh");
}

#[tokio::test]
async fn test_get_elements_name_falls_back_to_place_id() {
    let (server, config) = setup_mock().await;

    let only_place_id = serde_json::json!({
        "type": "Feature",
        "properties": {
            "place_id": "51abc123def"
        },
        "geometry": {
            "type": "Point",
            "coordinates": [-3.2, 55.9]
        }
    });

    let body = feature_collection(vec![only_place_id]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 55.9, -3.2, 500, None, Mode::Pubs)
        .await
        .expect("should succeed");

    assert_eq!(elements.len(), 1);
    assert_eq!(elements[0].names.name, "51abc123def");
}

#[tokio::test]
async fn test_get_elements_with_bearing_filters_correctly() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("North Pub", "n", 0.0, 1.0),  // bearing ~0 deg
        feature("East Pub", "e", 1.0, 0.0),   // bearing ~90 deg
        feature("South Pub", "s", 0.0, -1.0), // bearing ~180 deg
        feature("West Pub", "w", -1.0, 0.0),  // bearing ~270 deg
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 0.0, 0.0, 1000, Some(90.0), Mode::Pubs)
        .await
        .expect("should succeed");

    assert_eq!(elements.len(), 1);
    assert_eq!(elements[0].names.name, "East Pub");
}

#[tokio::test]
async fn test_get_elements_with_bearing_wraps_around_north() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("North Pub", "n", 0.0, 1.0), // bearing ~0 deg
        feature("NNE Pub", "nne", 0.2, 1.0), // bearing ~11 deg (within 35 deg of 350)
        feature("East Pub", "e", 1.0, 0.0),  // bearing ~90 deg (outside)
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 0.0, 0.0, 1000, Some(350.0), Mode::Pubs)
        .await
        .expect("should succeed");

    let names: HashSet<&str> = elements.iter().map(|e| e.names.name.as_str()).collect();
    assert!(
        names.contains("North Pub"),
        "North Pub should be in 350 deg cone"
    );
    assert!(
        names.contains("NNE Pub"),
        "NNE Pub should be in 350 deg cone"
    );
    assert!(
        !names.contains("East Pub"),
        "East Pub should not be in 350 deg cone"
    );
}

#[tokio::test]
async fn test_get_elements_sends_correct_categories_for_pubs() {
    let (server, config) = setup_mock().await;

    Mock::given(method("GET"))
        .and(query_param(
            "categories",
            "catering.pub,catering.bar,catering.biergarten,catering.taproom",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(feature_collection(vec![])))
        .expect(1)
        .mount(&server)
        .await;

    let _ = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs).await;
}

#[tokio::test]
async fn test_get_elements_sends_correct_categories_for_non_alcohol() {
    let (server, config) = setup_mock().await;

    Mock::given(method("GET"))
        .and(query_param("categories", "catering.cafe"))
        .respond_with(ResponseTemplate::new(200).set_body_json(feature_collection(vec![])))
        .expect(1)
        .mount(&server)
        .await;

    let _ = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::NonAlcohol).await;
}

#[tokio::test]
async fn test_get_random_pub_returns_one_result() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("Pub A", "id_a", -0.1, 51.5),
        feature("Pub B", "id_b", -0.11, 51.51),
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let result = roulette::get_random_pub(
        &config,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &[],
        &PlaceFilters::default(),
    )
    .await
    .expect("should succeed");

    assert!(result.name == "Pub A" || result.name == "Pub B");
    assert!(result.place_id.is_some());
}

#[tokio::test]
async fn test_get_random_pub_none_found() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let err = roulette::get_random_pub(
        &config,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &[],
        &PlaceFilters::default(),
    )
    .await
    .expect_err("should error on no results");

    let (status, message) = err.to_response();
    assert_eq!(status.as_u16(), 404);
    assert!(message.contains("No pubs found"));
}

#[tokio::test]
async fn test_get_random_pub_none_found_non_alcohol() {
    let (server, config) = setup_mock().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(feature_collection(vec![])))
        .mount(&server)
        .await;

    let err = roulette::get_random_pub(
        &config,
        51.5,
        -0.1,
        500,
        None,
        Mode::NonAlcohol,
        &[],
        &PlaceFilters::default(),
    )
    .await
    .expect_err("should error");

    let (_, message) = err.to_response();
    assert!(
        message.contains("coffee shops"),
        "error should mention coffee shops: {message}"
    );
}

#[tokio::test]
async fn test_get_random_pub_visited_filter_excludes() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("Pub A", "id_a", -0.1, 51.5),
        feature("Pub B", "id_b", -0.11, 51.51),
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let visited = vec!["id_a".to_string()];
    let result = roulette::get_random_pub(
        &config,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &visited,
        &PlaceFilters::default(),
    )
    .await
    .expect("should succeed");

    assert_eq!(result.name, "Pub B");
    assert_eq!(result.place_id.as_deref(), Some("id_b"));
}

#[tokio::test]
async fn test_get_random_pub_all_visited_returns_none_found() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("Pub A", "id_a", -0.1, 51.5),
        feature("Pub B", "id_b", -0.11, 51.51),
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let visited = vec!["id_a".to_string(), "id_b".to_string()];
    let err = roulette::get_random_pub(
        &config,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &visited,
        &PlaceFilters::default(),
    )
    .await
    .expect_err("should error when all are visited");

    let (status, _) = err.to_response();
    assert_eq!(status.as_u16(), 404);
}

#[tokio::test]
async fn test_get_random_pub_geoapify_error_returns_500() {
    let (server, config) = setup_mock().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let err = roulette::get_random_pub(
        &config,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &[],
        &PlaceFilters::default(),
    )
    .await
    .expect_err("should propagate upstream error");

    let (status, message) = err.to_response();
    assert_eq!(status.as_u16(), 500);
    assert_eq!(message, "An internal service error occurred.");
}

#[tokio::test]
async fn test_get_random_crawl_returns_requested_count() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("Pub A", "id_a", -0.1, 51.5),
        feature("Pub B", "id_b", -0.11, 51.51),
        feature("Pub C", "id_c", -0.09, 51.49),
        feature("Pub D", "id_d", -0.12, 51.52),
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let result = roulette::get_random_crawl(
        &config,
        3,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &[],
        &PlaceFilters::default(),
    )
    .await
    .expect("should succeed");

    assert_eq!(result.len(), 3);

    let ids: HashSet<&str> = result
        .iter()
        .filter_map(|r| r.place_id.as_deref())
        .collect();
    assert_eq!(ids.len(), 3, "crawl results should be unique");
}

#[tokio::test]
async fn test_get_random_crawl_not_enough_found() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("Pub A", "id_a", -0.1, 51.5),
        feature("Pub B", "id_b", -0.11, 51.51),
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let err = roulette::get_random_crawl(
        &config,
        5,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &[],
        &PlaceFilters::default(),
    )
    .await
    .expect_err("should error when not enough pubs");

    let (status, message) = err.to_response();
    assert_eq!(status.as_u16(), 404);
    assert!(message.contains("Not enough"));
    assert!(message.contains("5"));
    assert!(message.contains("2"));
}

#[tokio::test]
async fn test_get_random_crawl_none_found() {
    let (server, config) = setup_mock().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(feature_collection(vec![])))
        .mount(&server)
        .await;

    let err = roulette::get_random_crawl(
        &config,
        3,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &[],
        &PlaceFilters::default(),
    )
    .await
    .expect_err("should error on empty");

    let (status, message) = err.to_response();
    assert_eq!(status.as_u16(), 404);
    assert!(message.contains("No pubs found"));
}

#[tokio::test]
async fn test_get_random_crawl_visited_reduces_pool() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("Pub A", "id_a", -0.1, 51.5),
        feature("Pub B", "id_b", -0.11, 51.51),
        feature("Pub C", "id_c", -0.09, 51.49),
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let visited = vec!["id_a".to_string()];
    let result = roulette::get_random_crawl(
        &config,
        2,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &visited,
        &PlaceFilters::default(),
    )
    .await
    .expect("should succeed");

    assert_eq!(result.len(), 2);
    assert!(result.iter().all(|r| r.place_id.as_deref() != Some("id_a")));
}

#[tokio::test]
async fn test_get_random_crawl_visited_causes_not_enough() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![
        feature("Pub A", "id_a", -0.1, 51.5),
        feature("Pub B", "id_b", -0.11, 51.51),
        feature("Pub C", "id_c", -0.09, 51.49),
    ]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let visited = vec!["id_a".to_string(), "id_b".to_string()];
    let err = roulette::get_random_crawl(
        &config,
        3,
        51.5,
        -0.1,
        500,
        None,
        Mode::Pubs,
        &visited,
        &PlaceFilters::default(),
    )
    .await
    .expect_err("should error");

    let (status, message) = err.to_response();
    assert_eq!(status.as_u16(), 404);
    assert!(message.contains("Not enough"));
}

#[test]
fn test_app_error_validation_returns_400() {
    use validator::Validate;

    #[derive(Validate)]
    struct Dummy {
        #[validate(range(min = 1, max = 10))]
        value: i32,
    }

    let d = Dummy { value: 999 };
    let err = d.validate().unwrap_err();
    let app_err = AppError::from(err);
    let (status, message) = app_err.to_response();
    assert_eq!(status.as_u16(), 400);
    // The full validator output is logged, not returned, so the message must
    // stay generic.
    assert!(!message.contains("value"));
    assert!(!message.is_empty());
}

#[test]
fn test_app_error_parse_error_returns_400() {
    let err = AppError::ParseError("bad json".to_string());
    let (status, message) = err.to_response();
    assert_eq!(status.as_u16(), 400);
    assert!(message.contains("Failed to parse"));
    assert!(message.contains("bad json"));
}

#[test]
fn test_app_error_osm_error_returns_500_generic_message() {
    let err = AppError::OsmError("some internal detail".to_string());
    let (status, message) = err.to_response();
    assert_eq!(status.as_u16(), 500);
    assert!(!message.contains("internal detail"));
    assert_eq!(message, "An internal service error occurred.");
}

#[test]
fn test_app_error_not_enough_found() {
    let err = AppError::NotEnoughFound {
        requested: 5,
        found: 2,
        mode: Mode::Pubs,
    };
    let (status, message) = err.to_response();
    assert_eq!(status.as_u16(), 404);
    assert!(message.contains("5"));
    assert!(message.contains("2"));
    assert!(message.contains("pubs"));
}

#[test]
fn test_pub_response_serialization() {
    let resp = PubResponse {
        name: "Test Pub".to_string(),
        latitude: 51.5,
        longitude: -0.1,
        place_id: Some("test_id".to_string()),
        address_line1: Some("Test Pub".to_string()),
        address_line2: Some("123 Test Street".to_string()),
        phone: None,
        website: None,
        opening_hours: None,
        wikidata_id: None,
        distance_meters: None,
    };

    let json = serde_json::to_string(&resp).expect("should serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("should parse");

    assert_eq!(parsed["name"], "Test Pub");
    assert_eq!(parsed["latitude"], 51.5);
    assert_eq!(parsed["longitude"], -0.1);
    assert_eq!(parsed["place_id"], "test_id");
    assert_eq!(parsed["address_line1"], "Test Pub");
    assert_eq!(parsed["address_line2"], "123 Test Street");
}

#[test]
fn test_pub_response_deserialization() {
    let json = r#"{"name":"Pub","latitude":1.0,"longitude":2.0,"place_id":null,"address_line1":null,"address_line2":null}"#;
    let resp: PubResponse = serde_json::from_str(json).expect("should deserialize");
    assert_eq!(resp.name, "Pub");
    assert!(resp.place_id.is_none());
}

#[test]
fn test_mode_default_is_pubs() {
    assert_eq!(Mode::default(), Mode::Pubs);
}

#[test]
fn test_mode_serde_roundtrip() {
    let pubs_json = serde_json::to_string(&Mode::Pubs).unwrap();
    assert_eq!(pubs_json, "\"pubs\"");

    let na_json = serde_json::to_string(&Mode::NonAlcohol).unwrap();
    assert_eq!(na_json, "\"nonalcohol\"");

    let pubs: Mode = serde_json::from_str("\"pubs\"").unwrap();
    assert_eq!(pubs, Mode::Pubs);

    let na: Mode = serde_json::from_str("\"nonalcohol\"").unwrap();
    assert_eq!(na, Mode::NonAlcohol);
}

#[test]
fn test_mode_display() {
    assert_eq!(format!("{}", Mode::Pubs), "pubs");
    assert_eq!(format!("{}", Mode::NonAlcohol), "coffee shops");
}

#[test]
fn test_mode_labels() {
    assert_eq!(Mode::Pubs.label(), "pubs");
    assert_eq!(Mode::Pubs.label_singular(), "pub");
    assert_eq!(Mode::NonAlcohol.label(), "coffee shops");
    assert_eq!(Mode::NonAlcohol.label_singular(), "coffee shop");
}

#[tokio::test]
async fn test_realistic_geoapify_payload() {
    let (server, config) = setup_mock().await;

    // A full-fat Geoapify response: extra fields, conditions in categories,
    // float distances.
    let body = serde_json::json!({
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {
                    "name": "All Bar One",
                    "country": "United Kingdom",
                    "country_code": "gb",
                    "state": "Scotland",
                    "city": "City of Edinburgh",
                    "postcode": "EH3 9BY",
                    "district": "Lauriston",
                    "suburb": "Tollcross",
                    "street": "Lothian Road",
                    "housenumber": "50",
                    "iso3166_2": "GB-SCT",
                    "iso3166_2_sublevel": "GB-EDH",
                    "lon": -3.2069638,
                    "lat": 55.94749,
                    "state_code": "SCT",
                    "formatted": "All Bar One, 50 Lothian Road, City of Edinburgh, EH3 9BY, United Kingdom",
                    "address_line1": "All Bar One",
                    "address_line2": "50 Lothian Road, City of Edinburgh, EH3 9BY, United Kingdom",
                    "categories": [
                        "catering",
                        "catering.bar",
                        "catering.pub",
                        "building",
                        "building.catering"
                    ],
                    "datasource": {
                        "sourcename": "openstreetmap",
                        "attribution": "OpenStreetMap contributors"
                    },
                    "distance": 123,
                    "place_id": "51f07665abc123",
                    "osm_id": 12345678,
                    "osm_type": "w"
                },
                "geometry": {
                    "type": "Point",
                    "coordinates": [-3.2069638, 55.94749]
                }
            },
            {
                "type": "Feature",
                "properties": {
                    "name": "The Hanging Bat",
                    "country": "United Kingdom",
                    "country_code": "gb",
                    "state": "Scotland",
                    "city": "City of Edinburgh",
                    "postcode": 4042,
                    "street": "Lothian Road",
                    "housenumber": 133,
                    "lon": -3.2045,
                    "lat": 55.9455,
                    "formatted": "The Hanging Bat, 133 Lothian Road, City of Edinburgh",
                    "address_line1": "The Hanging Bat",
                    "address_line2": "133 Lothian Road, City of Edinburgh",
                    "categories": [
                        "catering",
                        "catering.pub",
                        "fee",
                        "internet_access.free",
                        "wheelchair.yes"
                    ],
                    "datasource": {
                        "sourcename": "openstreetmap"
                    },
                    "distance": 250,
                    "place_id": "51f07665def456",
                    "osm_id": 87654321,
                    "osm_type": "n"
                },
                "geometry": {
                    "type": "Point",
                    "coordinates": [-3.2045, 55.9455]
                }
            }
        ]
    });

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 55.946, -3.205, 500, None, Mode::Pubs)
        .await
        .expect("realistic payload must parse");

    assert_eq!(elements.len(), 2);

    let bar_one = elements
        .iter()
        .find(|e| e.names.name == "All Bar One")
        .unwrap();
    assert_eq!(bar_one.place_id.as_deref(), Some("51f07665abc123"));
    assert_eq!(bar_one.id, "12345678");
    assert!((bar_one.location.latitude - 55.94749).abs() < 1e-5);
    assert!((bar_one.location.longitude - (-3.2069638)).abs() < 1e-5);

    let bat = elements
        .iter()
        .find(|e| e.names.name == "The Hanging Bat")
        .unwrap();
    assert_eq!(bat.place_id.as_deref(), Some("51f07665def456"));
    assert_eq!(bat.id, "87654321");
    assert_eq!(bat.address_line1.as_deref(), Some("The Hanging Bat"));
}

#[tokio::test]
async fn test_alt_name_deduped_when_same_as_primary() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![serde_json::json!({
        "type": "Feature",
        "properties": {
            "name": "The Crown",
            "alt_name": "The Crown",
            "old_name": "The Old Crown",
            "place_id": "dup_test",
            "osm_id": 1
        },
        "geometry": {
            "type": "Point",
            "coordinates": [-0.1, 51.5]
        }
    })]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs)
        .await
        .expect("should succeed");

    assert_eq!(elements.len(), 1);
    assert_eq!(elements[0].names.name, "The Crown");
    assert!(elements[0].names.alt_name.is_none());
    assert_eq!(elements[0].names.old_name.as_deref(), Some("The Old Crown"));
}

#[tokio::test]
async fn test_old_name_deduped_when_same_as_primary() {
    let (server, config) = setup_mock().await;

    let body = feature_collection(vec![serde_json::json!({
        "type": "Feature",
        "properties": {
            "name": "The Crown",
            "old_name": "The Crown",
            "place_id": "dup_test2",
            "osm_id": 2
        },
        "geometry": {
            "type": "Point",
            "coordinates": [-0.1, 51.5]
        }
    })]);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let elements = places::get_elements_in_radius(&config, 51.5, -0.1, 500, None, Mode::Pubs)
        .await
        .expect("should succeed");

    assert_eq!(elements.len(), 1);
    assert!(elements[0].names.old_name.is_none());
}
