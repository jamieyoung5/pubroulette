use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;
use tracing::{debug, error, info};

const GEOAPIFY_PLACES_ENDPOINT: &str = "https://api.geoapify.com/v2/places";

/// Process-wide HTTP client. Timeouts keep a slow upstream from eating the
/// whole function budget, and reuse keeps the TLS pool warm between calls.
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .connect_timeout(Duration::from_secs(2))
        .user_agent(concat!(
            "pubroulette/",
            env!("CARGO_PKG_VERSION"),
            " (+https://pubroulette.com)"
        ))
        .build()
        .expect("reqwest client should build with default config")
});

/// Shared so other modules get the same pool and timeouts.
pub fn http_client() -> &'static reqwest::Client {
    &HTTP
}

/// Connection details for the Geoapify Places API. [`GeoapifyConfig::new`]
/// lets tests skip the environment.
#[derive(Debug, Clone)]
pub struct GeoapifyConfig {
    pub base_url: String,
    pub api_key: String,
}

impl GeoapifyConfig {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    /// Reads `GEOAPIFY_API_KEY`, and `GEOAPIFY_BASE_URL` if it's set.
    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("GEOAPIFY_API_KEY")
            .map_err(|_| "GEOAPIFY_API_KEY environment variable is not set".to_string())?;
        let base_url = std::env::var("GEOAPIFY_BASE_URL")
            .unwrap_or_else(|_| GEOAPIFY_PLACES_ENDPOINT.to_owned());
        Ok(Self { base_url, api_key })
    }
}

#[derive(Debug, Clone)]
pub struct Element {
    pub elem_type: String,
    pub id: String,
    pub place_id: Option<String>,
    pub location: Location,
    pub names: Names,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    /// Distance from the query origin in metres (if Geoapify provided it)
    pub distance_meters: Option<f64>,
    /// Whether the venue has outdoor seating / a beer garden
    pub outdoor_seating: bool,
    pub phone: Option<String>,
    pub website: Option<String>,
    /// Raw OSM-format opening hours string (e.g. "Mo-Fr 11:00-23:00; Sa-Su 12:00-00:00")
    pub opening_hours: Option<String>,
    /// Wikidata Q-number, e.g. "Q12345", this is useful for fetching photos / Wikipedia summaries
    pub wikidata_id: Option<String>,
    pub tags: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone)]
pub struct Names {
    pub name: String,
    pub alt_name: Option<String>,
    pub old_name: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Pubs,
    NonAlcohol,
}

impl Mode {
    pub fn categories(&self) -> &'static str {
        match self {
            Mode::Pubs => "catering.pub,catering.bar,catering.biergarten,catering.taproom",
            Mode::NonAlcohol => "catering.cafe",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Mode::Pubs => "pubs",
            Mode::NonAlcohol => "coffee shops",
        }
    }

    pub fn label_singular(&self) -> &'static str {
        match self {
            Mode::Pubs => "pub",
            Mode::NonAlcohol => "coffee shop",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Deserialize, Debug)]
struct RawGeometry {
    coordinates: [f64; 2],
}

#[derive(Deserialize, Debug)]
struct RawFeature {
    properties: serde_json::Value,
    geometry: RawGeometry,
}

#[derive(Deserialize)]
struct RawResponse {
    features: Vec<RawFeature>,
}

// Geoapify is inconsistent about types: housenumbers, postcodes and osm_ids
// arrive as either strings or numbers. These coerce either shape.

fn value_as_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn value_as_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn value_as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn insert_tag(tags: &mut HashMap<String, String>, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        tags.insert(key.to_owned(), v.to_owned());
    }
}

/// Try to turn a raw GeoJSON feature into an `Element`. Returns `None` for
/// features we can't find any usable name for, which get silently skipped.
fn parse_feature(raw: RawFeature) -> Option<Element> {
    let props = raw.properties.as_object()?;

    let place_id = props.get("place_id").and_then(value_as_string);
    let name = props.get("name").and_then(value_as_string);
    let formatted = props.get("formatted").and_then(value_as_string);
    let address_line1 = props.get("address_line1").and_then(value_as_string);
    let address_line2 = props.get("address_line2").and_then(value_as_string);
    let mut alt_name = props.get("alt_name").and_then(value_as_string);
    let mut old_name = props.get("old_name").and_then(value_as_string);
    let osm_id = props.get("osm_id").and_then(value_as_i64);
    let osm_type = props.get("osm_type").and_then(value_as_string);
    let distance_meters = props.get("distance").and_then(value_as_f64);
    // Enriched OSM fields arrive at the top level, under `contact`, or under
    // `datasource.raw`, depending on the field. Walk all three.
    let top = |key: &str| props.get(key).and_then(value_as_string);
    let contact = |key: &str| {
        props
            .get("contact")
            .and_then(|c| c.get(key))
            .and_then(value_as_string)
    };
    let datasource_raw = |key: &str| {
        props
            .get("datasource")
            .and_then(|d| d.get("raw"))
            .and_then(|r| r.get(key))
            .and_then(value_as_string)
    };
    let condition = |key: &str| {
        props
            .get("conditions")
            .and_then(|c| c.get(key))
            .and_then(value_as_string)
    };

    let outdoor_seating = condition("outdoor_seating")
        .or_else(|| datasource_raw("outdoor_seating"))
        .map(|s| s == "yes")
        .unwrap_or(false);

    let phone = top("phone")
        .or_else(|| contact("phone"))
        .or_else(|| datasource_raw("phone"));
    let website = top("website")
        .or_else(|| contact("website"))
        .or_else(|| datasource_raw("website"));
    let opening_hours = top("opening_hours").or_else(|| datasource_raw("opening_hours"));
    let wikidata_id = props
        .get("wiki_and_media")
        .and_then(|w| w.get("wikidata"))
        .and_then(value_as_string)
        .or_else(|| datasource_raw("wikidata"));

    // Fall back through the name candidates; skip the feature if none work.
    let primary_name = name
        .or_else(|| formatted.clone())
        .or_else(|| address_line1.clone())
        .or_else(|| address_line2.clone())
        .or_else(|| alt_name.clone())
        .or_else(|| old_name.clone())
        .or_else(|| place_id.clone())?;

    // Drop alt/old names identical to the primary.
    if alt_name.as_deref() == Some(&primary_name) {
        alt_name = None;
    }
    if old_name.as_deref() == Some(&primary_name) {
        old_name = None;
    }

    let id = osm_id
        .map(|n| n.to_string())
        .or_else(|| place_id.clone())
        .unwrap_or_else(|| "unknown".to_owned());

    let elem_type = osm_type.as_deref().unwrap_or("feature").to_owned();

    // Keep the rest as tags for debugging.
    let mut tags = HashMap::new();
    insert_tag(&mut tags, "place_id", place_id.as_deref());
    if let Some(v) = osm_id {
        tags.insert("osm_id".to_owned(), v.to_string());
    }
    insert_tag(&mut tags, "osm_type", osm_type.as_deref());
    insert_tag(&mut tags, "formatted", formatted.as_deref());
    insert_tag(&mut tags, "address_line1", address_line1.as_deref());
    insert_tag(&mut tags, "address_line2", address_line2.as_deref());
    if let Some(d) = distance_meters {
        tags.insert("distance".to_owned(), d.to_string());
    }

    let [lon, lat] = raw.geometry.coordinates;

    Some(Element {
        elem_type,
        id,
        tags,
        names: Names {
            name: primary_name,
            alt_name,
            old_name,
        },
        place_id,
        address_line1,
        address_line2,
        distance_meters,
        outdoor_seating,
        phone,
        website,
        opening_hours,
        wikidata_id,
        location: Location {
            latitude: lat,
            longitude: lon,
        },
    })
}

/// Max results Geoapify will return per call. A response this long may be
/// truncated, so callers can't treat it as a complete picture of the area.
pub const RESULT_LIMIT: usize = 200;

/// Fetch places from Geoapify within the given radius, optionally filtered to
/// a directional cone if `bearing` is provided.
pub async fn get_elements_in_radius(
    config: &GeoapifyConfig,
    latitude: f64,
    longitude: f64,
    radius: i16,
    bearing: Option<f64>,
    mode: Mode,
) -> Result<Vec<Element>, String> {
    let categories = mode.categories();
    let filter = format!("circle:{longitude},{latitude},{radius}");
    let bias = format!("proximity:{longitude},{latitude}");
    let limit = RESULT_LIMIT.to_string();

    info!(
        base_url = %config.base_url,
        %categories,
        mode = %mode,
        latitude,
        longitude,
        radius,
        "geoapify_request"
    );

    let response = HTTP
        .get(&config.base_url)
        .query(&[
            ("categories", categories),
            ("filter", filter.as_str()),
            ("bias", bias.as_str()),
            ("limit", limit.as_str()),
            ("apiKey", config.api_key.as_str()),
        ])
        .send()
        .await
        .map_err(|e| {
            // Split the log so a slow upstream is distinguishable from a
            // broken response.
            if e.is_timeout() {
                error!(error = %e, "geoapify_timeout");
            } else {
                error!(error = %e, "geoapify_request_error");
            }
            e.to_string()
        })?;

    info!(status = %response.status(), "geoapify_response");

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        error!(body = %body, "geoapify_upstream_error");
        return Err(format!(
            "Geoapify Places API returned a non-success status: {body}"
        ));
    }

    let body = response.text().await.map_err(|e| e.to_string())?;
    debug!(
        body_preview = &body[..body.len().min(500)],
        body_len = body.len(),
        "geoapify_response_body"
    );

    let raw: RawResponse = serde_json::from_str(&body).map_err(|e| {
        error!(
            error = %e,
            body_preview = &body[..body.len().min(500)],
            "geoapify_json_parse_error"
        );
        e.to_string()
    })?;

    info!(
        feature_count = raw.features.len(),
        "geoapify_features_received"
    );

    let elements: Vec<Element> = raw.features.into_iter().filter_map(parse_feature).collect();

    match bearing {
        Some(b) => Ok(filter_by_bearing(elements, latitude, longitude, b)),
        None => Ok(elements),
    }
}

pub fn filter_by_bearing(elements: Vec<Element>, lat: f64, lon: f64, bearing: f64) -> Vec<Element> {
    elements
        .into_iter()
        .filter(|el| {
            let el_bearing =
                calculate_bearing(lat, lon, el.location.latitude, el.location.longitude);
            is_within_cone(el_bearing, bearing, 70.0)
        })
        .collect()
}

fn calculate_bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let lat1 = lat1.to_radians();
    let lat2 = lat2.to_radians();
    let dlon = (lon2 - lon1).to_radians();

    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();

    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

fn is_within_cone(bearing: f64, target: f64, cone_width: f64) -> bool {
    let half = cone_width / 2.0;
    let mut diff = (bearing - target).rem_euclid(360.0);
    if diff > 180.0 {
        diff -= 360.0;
    }
    diff.abs() <= half
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_mock_element(id: i64, lat: f64, lon: f64) -> Element {
        Element {
            elem_type: "node".to_owned(),
            id: id.to_string(),
            location: Location {
                latitude: lat,
                longitude: lon,
            },
            names: Names {
                name: "Test Pub".to_owned(),
                alt_name: None,
                old_name: None,
            },
            place_id: None,
            address_line1: None,
            address_line2: None,
            distance_meters: None,
            outdoor_seating: false,
            phone: None,
            website: None,
            opening_hours: None,
            wikidata_id: None,
            tags: HashMap::new(),
        }
    }

    /// Parse GeoJSON into `Element`s.
    fn parse_geojson(json: &str) -> Vec<Element> {
        let raw: RawResponse = serde_json::from_str(json).expect("should parse without error");
        raw.features.into_iter().filter_map(parse_feature).collect()
    }

    /// Parse GeoJSON and return the raw features for low-level assertions.
    fn parse_raw(json: &str) -> Vec<RawFeature> {
        let raw: RawResponse = serde_json::from_str(json).expect("should parse without error");
        raw.features
    }

    #[test]
    fn test_value_as_string() {
        assert_eq!(
            value_as_string(&serde_json::json!("hello")),
            Some("hello".to_owned())
        );
        assert_eq!(
            value_as_string(&serde_json::json!(4042)),
            Some("4042".to_owned())
        );
        assert_eq!(
            value_as_string(&serde_json::json!(2.72)),
            Some("2.72".to_owned())
        );
        assert_eq!(
            value_as_string(&serde_json::json!(true)),
            Some("true".to_owned())
        );
        assert_eq!(value_as_string(&serde_json::Value::Null), None);
        assert_eq!(value_as_string(&serde_json::json!(["a", "b"])), None);
        assert_eq!(value_as_string(&serde_json::json!({"key": "val"})), None);
    }

    #[test]
    fn test_value_as_i64() {
        assert_eq!(value_as_i64(&serde_json::json!(12345)), Some(12345));
        assert_eq!(value_as_i64(&serde_json::json!("67890")), Some(67890));
        assert_eq!(value_as_i64(&serde_json::json!("not_a_number")), None);
        assert_eq!(value_as_i64(&serde_json::Value::Null), None);
    }

    #[test]
    fn test_calculate_bearing() {
        let precision = 1e-3;

        let north = calculate_bearing(0.0, 0.0, 1.0, 0.0);
        assert!((north - 0.0).abs() < precision);

        let east = calculate_bearing(0.0, 0.0, 0.0, 1.0);
        assert!((east - 90.0).abs() < precision);

        let south = calculate_bearing(0.0, 0.0, -1.0, 0.0);
        assert!((south - 180.0).abs() < precision);

        let west = calculate_bearing(0.0, 0.0, 0.0, -1.0);
        assert!((west - 270.0).abs() < precision);

        let ne = calculate_bearing(51.5, -0.1, 52.5, 0.9);
        assert!((ne - 31.226).abs() < precision);
    }

    #[test]
    fn test_is_within_cone() {
        let width = 70.0;

        assert!(is_within_cone(90.0, 90.0, width));
        assert!(is_within_cone(55.0, 90.0, width));
        assert!(is_within_cone(125.0, 90.0, width));
        assert!(!is_within_cone(54.9, 90.0, width));
        assert!(!is_within_cone(125.1, 90.0, width));

        // Wraps around 0.
        assert!(is_within_cone(0.0, 0.0, width));
        assert!(is_within_cone(360.0, 0.0, width));
        assert!(is_within_cone(35.0, 0.0, width));
        assert!(is_within_cone(325.0, 0.0, width));
        assert!(is_within_cone(325.0, 360.0, width));
        assert!(!is_within_cone(35.1, 0.0, width));
        assert!(!is_within_cone(324.9, 0.0, width));

        // Wraps the other way.
        assert!(is_within_cone(340.0, 350.0, width));
        assert!(is_within_cone(10.0, 350.0, width));
        assert!(is_within_cone(25.0, 350.0, width));
        assert!(is_within_cone(315.0, 350.0, width));
        assert!(!is_within_cone(25.1, 350.0, width));
        assert!(!is_within_cone(314.9, 350.0, width));
    }

    #[test]
    fn test_filter_by_bearing() {
        let el_north = create_mock_element(1, 1.0, 0.0);
        let el_east = create_mock_element(2, 0.0, 1.0);
        let el_south = create_mock_element(3, -1.0, 0.0);
        let el_west = create_mock_element(4, 0.0, -1.0);
        let elements = vec![el_north, el_east, el_south, el_west];

        let north = filter_by_bearing(elements.clone(), 0.0, 0.0, 0.0);
        assert_eq!(north.len(), 1);
        assert_eq!(north[0].id, "1");

        let east = filter_by_bearing(elements.clone(), 0.0, 0.0, 90.0);
        assert_eq!(east.len(), 1);
        assert_eq!(east[0].id, "2");

        let ne = filter_by_bearing(elements.clone(), 0.0, 0.0, 45.0);
        assert_eq!(ne.len(), 0);
    }

    #[test]
    fn test_deserialize_geoapify_response_with_integer_housenumber() {
        let json = r#"{
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
                        "lon": -3.2069638,
                        "lat": 55.94749,
                        "formatted": "All Bar One, 50 Lothian Road, City of Edinburgh, EH3 9BY, United Kingdom",
                        "address_line1": "All Bar One",
                        "address_line2": "50 Lothian Road, City of Edinburgh, EH3 9BY, United Kingdom",
                        "categories": ["catering.pub", "catering.bar"],
                        "place_id": "abc123",
                        "osm_id": 12345,
                        "osm_type": "node"
                    },
                    "geometry": {
                        "type": "Point",
                        "coordinates": [-3.2069638, 55.94749]
                    }
                },
                {
                    "type": "Feature",
                    "properties": {
                        "name": "Some Pub",
                        "country": "United Kingdom",
                        "street": "High Street",
                        "housenumber": 4042,
                        "lon": -3.19,
                        "lat": 55.95,
                        "formatted": "Some Pub, 4042 High Street, Edinburgh",
                        "address_line1": "Some Pub",
                        "address_line2": "4042 High Street, Edinburgh",
                        "categories": ["catering.pub"],
                        "place_id": "def456",
                        "osm_id": 67890,
                        "osm_type": "way"
                    },
                    "geometry": {
                        "type": "Point",
                        "coordinates": [-3.19, 55.95]
                    }
                }
            ]
        }"#;

        let raw_features = parse_raw(json);
        assert_eq!(raw_features.len(), 2);

        for feature in &raw_features {
            let props = feature.properties.as_object().unwrap();
            assert!(props.get("name").and_then(value_as_string).is_some());
            assert!(props.get("place_id").and_then(value_as_string).is_some());
            assert!(props.get("osm_id").and_then(value_as_i64).is_some());
        }

        let props1 = raw_features[1].properties.as_object().unwrap();
        let housenumber = props1.get("housenumber").and_then(value_as_string);
        assert_eq!(housenumber, Some("4042".to_owned()));

        let elements = parse_geojson(json);
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].names.name, "All Bar One");
        assert_eq!(elements[1].names.name, "Some Pub");
    }

    #[test]
    fn test_deserialize_geoapify_response_with_missing_and_extra_fields() {
        let json = r#"{
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {
                        "formatted": "Unnamed Place, 123 Some Street",
                        "address_line1": "Unnamed Place",
                        "categories": ["catering.pub", "fee", "internet_access.free"],
                        "place_id": "xyz789",
                        "distance": 42,
                        "some_future_field": {"nested": true},
                        "another_new_field": [1, 2, 3]
                    },
                    "geometry": {
                        "type": "Point",
                        "coordinates": [-0.1, 51.5]
                    }
                }
            ]
        }"#;

        let raw_features = parse_raw(json);
        assert_eq!(raw_features.len(), 1);

        let props = raw_features[0].properties.as_object().unwrap();
        assert_eq!(props.get("name").and_then(value_as_string), None);
        assert_eq!(props.get("osm_id").and_then(value_as_i64), None);
        assert_eq!(
            props.get("place_id").and_then(value_as_string),
            Some("xyz789".to_owned())
        );
        assert_eq!(
            props.get("formatted").and_then(value_as_string),
            Some("Unnamed Place, 123 Some Street".to_owned())
        );

        let elements = parse_geojson(json);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].names.name, "Unnamed Place, 123 Some Street");
    }

    #[test]
    fn test_deserialize_feature_with_no_identifiable_name() {
        let json = r#"{
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {
                        "categories": ["catering.pub"]
                    },
                    "geometry": {
                        "type": "Point",
                        "coordinates": [-0.1, 51.5]
                    }
                }
            ]
        }"#;

        let elements = parse_geojson(json);
        assert!(elements.is_empty());

        // The feature parsed; it just has no usable name.
        let raw_features = parse_raw(json);
        assert_eq!(raw_features.len(), 1);

        let props = raw_features[0].properties.as_object().unwrap();
        let primary_name = props
            .get("name")
            .and_then(value_as_string)
            .or_else(|| props.get("formatted").and_then(value_as_string))
            .or_else(|| props.get("address_line1").and_then(value_as_string))
            .or_else(|| props.get("address_line2").and_then(value_as_string))
            .or_else(|| props.get("alt_name").and_then(value_as_string))
            .or_else(|| props.get("old_name").and_then(value_as_string))
            .or_else(|| props.get("place_id").and_then(value_as_string));
        assert_eq!(primary_name, None);
    }

    #[test]
    fn test_deserialize_with_numeric_postcode_and_distance() {
        let json = r#"{
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {
                        "name": "Test Pub",
                        "postcode": 12345,
                        "distance": 150.7,
                        "place_id": "test123",
                        "osm_id": "999",
                        "categories": ["catering.pub"]
                    },
                    "geometry": {
                        "type": "Point",
                        "coordinates": [0.0, 0.0]
                    }
                }
            ]
        }"#;

        let raw_features = parse_raw(json);
        let props = raw_features[0].properties.as_object().unwrap();

        assert_eq!(props.get("osm_id").and_then(value_as_i64), Some(999));
        assert_eq!(
            props.get("name").and_then(value_as_string),
            Some("Test Pub".to_owned())
        );

        let elements = parse_geojson(json);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].names.name, "Test Pub");
        assert_eq!(elements[0].id, "999");
    }
}
