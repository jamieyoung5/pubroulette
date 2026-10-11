//! Redis cache in front of the Geoapify Places API.
//!
//! Places are bucketed into H3 hexes. Per mode, `{m}:ledger:{hex}` marks a hex
//! as already fetched, `{m}:hex:{hex}` holds its place ids, and
//! `{m}:place:{id}` holds the place blob. The ledger is the only freshness
//! signal; any Redis or geometry failure falls back to a direct Geoapify call.

use crate::places::{self, Element, GeoapifyConfig, Location, Mode, Names};
use crate::rooms;
use h3o::{CellIndex, LatLng, Resolution};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tracing::{info, warn};

/// ~0.74 sq km per hex, so a 500 m search covers a handful and the 5 km max
/// covers a few hundred.
const HEX_RES: Resolution = Resolution::Eight;

/// Edge length, which for a hexagon is also the centre-to-vertex distance.
const HEX_EDGE_M: f64 = HEX_RES.edge_length_m();

/// Distance between neighbouring hex centres: `edge * sqrt(3)`.
const HEX_SPACING_M: f64 = HEX_EDGE_M * 1.732_050_8;

/// Pubs open and close rarely, so a month of staleness buys a lot of saved
/// upstream calls.
const LEDGER_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Places and hex sets outlive the ledger that gates them, so a live ledger
/// entry can never point at evicted data. Abandoned regions still expire.
const DATA_TTL_SECONDS: u64 = 2 * LEDGER_TTL_SECONDS;
const _: () = assert!(DATA_TTL_SECONDS > LEDGER_TTL_SECONDS);

/// Cap on the grown upstream query so a bad radius can't burn quota.
const MAX_QUERY_RADIUS_M: i16 = 7000;

/// Set to `1` or `true` to bypass the cache entirely. A kill switch if the
/// cache ever misbehaves, and what keeps the hermetic tests off a local Redis.
const DISABLE_ENV: &str = "PLACES_CACHE_DISABLED";

fn disabled() -> bool {
    matches!(
        std::env::var(DISABLE_ENV).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

/// Stored form of a place. Drops `tags`, which no caller reads. New fields
/// must be `#[serde(default)]` so old blobs still parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedPlace {
    place_id: String,
    name: String,
    latitude: f64,
    longitude: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    address_line1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    address_line2: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    website: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    opening_hours: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wikidata_id: Option<String>,
    #[serde(default)]
    outdoor_seating: bool,
}

impl CachedPlace {
    fn from_element(e: &Element) -> Option<Self> {
        let place_id = e.place_id.clone()?;
        Some(Self {
            place_id,
            name: e.names.name.clone(),
            latitude: e.location.latitude,
            longitude: e.location.longitude,
            address_line1: e.address_line1.clone(),
            address_line2: e.address_line2.clone(),
            phone: e.phone.clone(),
            website: e.website.clone(),
            opening_hours: e.opening_hours.clone(),
            wikidata_id: e.wikidata_id.clone(),
            outdoor_seating: e.outdoor_seating,
        })
    }

    /// `distance_meters` is recomputed per query because one blob serves
    /// queries from many origins.
    fn into_element(self, query_lat: f64, query_lon: f64) -> Element {
        let dist = haversine_m(query_lat, query_lon, self.latitude, self.longitude);
        Element {
            elem_type: "feature".to_string(),
            id: self.place_id.clone(),
            place_id: Some(self.place_id),
            location: Location {
                latitude: self.latitude,
                longitude: self.longitude,
            },
            names: Names {
                name: self.name,
                alt_name: None,
                old_name: None,
            },
            address_line1: self.address_line1,
            address_line2: self.address_line2,
            distance_meters: Some(dist),
            outdoor_seating: self.outdoor_seating,
            phone: self.phone,
            website: self.website,
            opening_hours: self.opening_hours,
            wikidata_id: self.wikidata_id,
            tags: std::collections::HashMap::new(),
        }
    }
}

/// Cached equivalent of [`places::get_elements_in_radius`]: same arguments,
/// same `Element` shape, same bearing filtering.
pub async fn get_places_in_radius(
    config: &GeoapifyConfig,
    latitude: f64,
    longitude: f64,
    radius: i16,
    bearing: Option<f64>,
    mode: Mode,
) -> Result<Vec<Element>, String> {
    let conn = if disabled() {
        None
    } else {
        match rooms::connect().await {
            Ok(c) => Some(c),
            Err(e) => {
                warn!(error = ?e, "places_cache_redis_unavailable");
                None
            }
        }
    };
    get_places_in_radius_with_conn(config, latitude, longitude, radius, bearing, mode, conn).await
}

/// As [`get_places_in_radius`], but with a caller-supplied connection. `None`
/// goes straight to Geoapify; tests use it to exercise the Redis-down path.
#[allow(clippy::too_many_arguments)]
pub async fn get_places_in_radius_with_conn(
    config: &GeoapifyConfig,
    latitude: f64,
    longitude: f64,
    radius: i16,
    bearing: Option<f64>,
    mode: Mode,
    conn: Option<redis::aio::MultiplexedConnection>,
) -> Result<Vec<Element>, String> {
    let radius = radius.clamp(1, MAX_QUERY_RADIUS_M);

    let Some(mut conn) = conn else {
        return places::get_elements_in_radius(config, latitude, longitude, radius, bearing, mode)
            .await;
    };

    match cached(&mut conn, config, latitude, longitude, radius, mode).await {
        Ok(elements) => {
            info!(served = elements.len(), radius, mode = %mode, "places_cache_served");
            Ok(match bearing {
                Some(b) => places::filter_by_bearing(elements, latitude, longitude, b),
                None => elements,
            })
        }
        Err(e) => {
            warn!(error = %e, mode = %mode, "places_cache_fallback");
            places::get_elements_in_radius(config, latitude, longitude, radius, bearing, mode).await
        }
    }
}

/// Serve the query from Redis, fetching and storing any hex we haven't seen.
/// Errors are stage-tagged strings; the caller turns them into a fallback.
async fn cached(
    conn: &mut redis::aio::MultiplexedConnection,
    config: &GeoapifyConfig,
    latitude: f64,
    longitude: f64,
    radius: i16,
    mode: Mode,
) -> Result<Vec<Element>, String> {
    let cover = cover_hexes(latitude, longitude, radius).map_err(|e| format!("cover: {e}"))?;

    let ledger_keys: Vec<String> = cover.iter().map(|h| ledger_key(mode, *h)).collect();
    let present = pipeline_exists(conn, &ledger_keys)
        .await
        .map_err(|e| format!("ledger: {e}"))?;
    let missing: Vec<CellIndex> = cover
        .iter()
        .zip(present.iter())
        .filter_map(|(h, present)| (!*present).then_some(*h))
        .collect();

    info!(
        cover_hexes = cover.len(),
        cache_misses = missing.len(),
        mode = %mode,
        "places_cache_lookup"
    );

    if !missing.is_empty() {
        populate(conn, config, latitude, longitude, radius, mode, &missing).await?;
    }

    let hex_keys: Vec<String> = cover.iter().map(|h| hex_key(mode, *h)).collect();
    let place_ids: HashSet<String> = conn
        .sunion(&hex_keys)
        .await
        .map_err(|e| format!("sunion: {e}"))?;
    if place_ids.is_empty() {
        return Ok(Vec::new());
    }

    let place_keys: Vec<String> = place_ids.iter().map(|id| place_key(mode, id)).collect();
    let blobs: Vec<Option<String>> = conn
        .mget(&place_keys)
        .await
        .map_err(|e| format!("mget: {e}"))?;

    // The hex cover overshoots the circle, so trim to the real radius.
    let radius_f = radius as f64;
    Ok(blobs
        .into_iter()
        .flatten()
        .filter_map(|s| serde_json::from_str::<CachedPlace>(&s).ok())
        .filter(|p| haversine_m(latitude, longitude, p.latitude, p.longitude) <= radius_f)
        .map(|p| p.into_element(latitude, longitude))
        .collect())
}

/// One `EXISTS` per key, pipelined. Output order matches input.
async fn pipeline_exists(
    conn: &mut redis::aio::MultiplexedConnection,
    keys: &[String],
) -> Result<Vec<bool>, redis::RedisError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut pipe = redis::pipe();
    for k in keys {
        pipe.cmd("EXISTS").arg(k);
    }
    let raw: Vec<i64> = pipe.query_async(conn).await?;
    Ok(raw.into_iter().map(|n| n == 1).collect())
}

/// Fetch every place enclosing the missing hexes in one upstream call, store
/// the results by hex, and credit each missing hex in the ledger.
#[allow(clippy::too_many_arguments)]
async fn populate(
    conn: &mut redis::aio::MultiplexedConnection,
    config: &GeoapifyConfig,
    user_lat: f64,
    user_lon: f64,
    user_radius: i16,
    mode: Mode,
    missing: &[CellIndex],
) -> Result<(), String> {
    let query_radius = enclosing_query_radius(user_lat, user_lon, user_radius, missing);

    info!(
        missing = missing.len(),
        user_radius, query_radius, mode = %mode, "places_cache_fetching"
    );

    // No bearing: these results serve later queries from any direction.
    let elements =
        places::get_elements_in_radius(config, user_lat, user_lon, query_radius, None, mode)
            .await?;

    // A full response may be truncated, so it can't be trusted to be the
    // whole story for any hex. Only rebuild sets from a complete response;
    // otherwise merge and leave departed places in place.
    let complete = elements.len() < places::RESULT_LIMIT;

    let mut pipe = redis::pipe();
    pipe.atomic();
    if complete {
        for hex in missing {
            pipe.cmd("DEL").arg(hex_key(mode, *hex)).ignore();
        }
    }

    let mut stored = 0usize;
    for el in &elements {
        let Some(place) = CachedPlace::from_element(el) else {
            continue; // No place_id, so nothing to dedup or vote on.
        };
        let Ok(json) = serde_json::to_string(&place) else {
            continue;
        };
        let Ok(ll) = LatLng::new(place.latitude, place.longitude) else {
            continue;
        };
        let owning_hex = hex_key(mode, ll.to_cell(HEX_RES));
        pipe.cmd("SET")
            .arg(place_key(mode, &place.place_id))
            .arg(&json)
            .arg("EX")
            .arg(DATA_TTL_SECONDS)
            .ignore();
        pipe.cmd("SADD")
            .arg(&owning_hex)
            .arg(&place.place_id)
            .ignore();
        pipe.cmd("EXPIRE")
            .arg(&owning_hex)
            .arg(DATA_TTL_SECONDS)
            .ignore();
        stored += 1;
    }

    // Credit every missing hex, including the empty ones. Caching the absence
    // of places is what stops cold rural hexes being re-fetched forever.
    for hex in missing {
        pipe.cmd("SET")
            .arg(ledger_key(mode, *hex))
            .arg("1")
            .arg("EX")
            .arg(LEDGER_TTL_SECONDS)
            .ignore();
    }

    pipe.query_async::<()>(conn)
        .await
        .map_err(|e| format!("populate: {e}"))?;

    info!(
        missing_hexes = missing.len(),
        upstream_returned = elements.len(),
        stored,
        rebuilt = complete,
        mode = %mode,
        "places_cache_populated"
    );
    Ok(())
}

/// Grow the user's radius until it encloses every missing hex.
fn enclosing_query_radius(
    user_lat: f64,
    user_lon: f64,
    user_radius: i16,
    missing: &[CellIndex],
) -> i16 {
    let mut needed = user_radius as f64;
    for hex in missing {
        let centre = LatLng::from(*hex);
        let farthest = haversine_m(user_lat, user_lon, centre.lat(), centre.lng()) + HEX_EDGE_M;
        needed = needed.max(farthest);
    }
    // Margin so float wobble can't leave us a metre short.
    needed += 50.0;
    (needed.min(MAX_QUERY_RADIUS_M as f64)) as i16
}

/// Hexes covering the query circle, plus one ring so a circle sitting on a
/// boundary still picks up its neighbours.
fn cover_hexes(
    lat: f64,
    lon: f64,
    radius_m: i16,
) -> Result<Vec<CellIndex>, h3o::error::InvalidLatLng> {
    let centre = LatLng::new(lat, lon)?.to_cell(HEX_RES);
    let steps = ((radius_m as f64 / HEX_SPACING_M).ceil() as u32).saturating_add(1);
    Ok(centre.grid_disk::<Vec<_>>(steps))
}

fn key_prefix(mode: Mode) -> &'static str {
    match mode {
        Mode::Pubs => "pubs",
        Mode::NonAlcohol => "cafes",
    }
}

fn ledger_key(mode: Mode, h: CellIndex) -> String {
    format!("{}:ledger:{h}", key_prefix(mode))
}

fn hex_key(mode: Mode, h: CellIndex) -> String {
    format!("{}:hex:{h}", key_prefix(mode))
}

fn place_key(mode: Mode, place_id: &str) -> String {
    format!("{}:place:{place_id}", key_prefix(mode))
}

/// Great-circle distance in metres. Duplicated from `roulette` because that
/// copy is private; lift to a shared module if a third caller appears.
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().atan2((1.0 - a).sqrt())
}

// Key helpers for integration tests, so they can clean up exactly what a
// query touched instead of flushing a shared Redis. Not for production use.

/// `(ledger_keys, hex_keys)` a query at this origin would touch.
#[doc(hidden)]
pub fn _test_keys_for(lat: f64, lon: f64, radius: i16, mode: Mode) -> (Vec<String>, Vec<String>) {
    let cover = cover_hexes(lat, lon, radius).unwrap_or_default();
    let ledger = cover.iter().map(|h| ledger_key(mode, *h)).collect();
    let hex = cover.iter().map(|h| hex_key(mode, *h)).collect();
    (ledger, hex)
}

#[doc(hidden)]
pub fn _test_place_key(mode: Mode, place_id: &str) -> String {
    place_key(mode, place_id)
}

/// Centre of the hex containing this point. Tests seed from the centre so a
/// random origin can't straddle a boundary and change the cover.
#[doc(hidden)]
pub fn _test_cell_centre(lat: f64, lon: f64) -> (f64, f64) {
    let cell = LatLng::new(lat, lon)
        .expect("test seed coordinate must be valid")
        .to_cell(HEX_RES);
    let centre = LatLng::from(cell);
    (centre.lat(), centre.lng())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_hexes_includes_centre() {
        let cover = cover_hexes(55.94, -3.18, 500).expect("cover");
        let expected = LatLng::new(55.94, -3.18).unwrap().to_cell(HEX_RES);
        assert!(cover.contains(&expected));
    }

    #[test]
    fn cover_hexes_grows_with_radius() {
        let small = cover_hexes(55.94, -3.18, 100).expect("cover");
        let big = cover_hexes(55.94, -3.18, 5000).expect("cover");
        assert!(big.len() > small.len());
    }

    #[test]
    fn cover_hexes_reaches_every_point_in_the_circle() {
        // Under-covering silently drops places, so check the cover extends
        // past the circle in the worst direction.
        for radius in [100i16, 500, 1500, 5000, MAX_QUERY_RADIUS_M] {
            let cover = cover_hexes(55.94, -3.18, radius).expect("cover");
            let reach = cover
                .iter()
                .map(|h| {
                    let c = LatLng::from(*h);
                    haversine_m(55.94, -3.18, c.lat(), c.lng())
                })
                .fold(0.0f64, f64::max);
            assert!(
                reach + HEX_EDGE_M >= radius as f64,
                "radius {radius} reaches only {reach}m"
            );
        }
    }

    #[test]
    fn cover_hexes_bounded_for_max_radius() {
        // Guards against a resolution or spacing bug shipping a huge SUNION.
        let cover = cover_hexes(55.94, -3.18, MAX_QUERY_RADIUS_M).expect("cover");
        assert!(cover.len() < 400, "cover blew up to {} hexes", cover.len());
    }

    #[test]
    fn enclosing_query_radius_covers_all_missing() {
        let far = LatLng::new(55.96, -3.18).unwrap().to_cell(HEX_RES);
        let grown = enclosing_query_radius(55.94, -3.18, 500, std::slice::from_ref(&far));
        let centre = LatLng::from(far);
        let d = haversine_m(55.94, -3.18, centre.lat(), centre.lng());
        assert!((grown as f64) >= d + HEX_EDGE_M);
    }

    #[test]
    fn enclosing_query_radius_respects_max() {
        let antipodal = LatLng::new(-55.94, 176.82).unwrap().to_cell(HEX_RES);
        let grown = enclosing_query_radius(55.94, -3.18, 500, std::slice::from_ref(&antipodal));
        assert_eq!(grown, MAX_QUERY_RADIUS_M);
    }

    #[test]
    fn keys_are_namespaced_per_mode() {
        let hex = LatLng::new(55.94, -3.18).unwrap().to_cell(HEX_RES);
        assert_ne!(
            ledger_key(Mode::Pubs, hex),
            ledger_key(Mode::NonAlcohol, hex)
        );
        assert_ne!(hex_key(Mode::Pubs, hex), hex_key(Mode::NonAlcohol, hex));
        assert_ne!(
            place_key(Mode::Pubs, "abc"),
            place_key(Mode::NonAlcohol, "abc")
        );
    }

    #[test]
    fn cached_place_round_trip_drops_no_essentials() {
        let original = CachedPlace {
            place_id: "abc".to_string(),
            name: "The Ship Inn".to_string(),
            latitude: 55.94,
            longitude: -3.18,
            address_line1: Some("1 High St".to_string()),
            address_line2: Some("Edinburgh".to_string()),
            phone: Some("+44 131 555 5555".to_string()),
            website: Some("https://example.co.uk".to_string()),
            opening_hours: Some("Mo-Fr 11:00-23:00".to_string()),
            wikidata_id: Some("Q1".to_string()),
            outdoor_seating: true,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: CachedPlace = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.place_id, original.place_id);
        assert!(parsed.outdoor_seating);

        let el = parsed.into_element(55.95, -3.19);
        assert!(el.distance_meters.unwrap() > 0.0);
        assert_eq!(el.place_id.as_deref(), Some("abc"));
    }

    #[test]
    fn cached_place_tolerates_blob_missing_optional_fields() {
        let legacy = r#"{
            "place_id": "abc",
            "name": "The Ship Inn",
            "latitude": 55.94,
            "longitude": -3.18
        }"#;
        let parsed: CachedPlace = serde_json::from_str(legacy).expect("legacy parses");
        assert_eq!(parsed.place_id, "abc");
        assert!(!parsed.outdoor_seating);
        assert!(parsed.phone.is_none());
    }
}
