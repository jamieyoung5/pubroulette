use crate::places::{Element, GeoapifyConfig, Mode};
use rand::distributions::WeightedIndex;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use vercel_runtime::StatusCode as VercelStatusCode;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubResponse {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub place_id: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opening_hours: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wikidata_id: Option<String>,
    /// Distance from the query origin, kept so rooms can resample by distance
    /// without another upstream call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance_meters: Option<f64>,
}

impl From<Element> for PubResponse {
    fn from(e: Element) -> Self {
        Self {
            name: e.names.name,
            latitude: e.location.latitude,
            longitude: e.location.longitude,
            place_id: e.place_id,
            address_line1: e.address_line1,
            address_line2: e.address_line2,
            phone: e.phone,
            website: e.website,
            opening_hours: e.opening_hours,
            wikidata_id: e.wikidata_id,
            distance_meters: e.distance_meters,
        }
    }
}

impl From<&Element> for PubResponse {
    fn from(e: &Element) -> Self {
        Self {
            name: e.names.name.clone(),
            latitude: e.location.latitude,
            longitude: e.location.longitude,
            place_id: e.place_id.clone(),
            address_line1: e.address_line1.clone(),
            address_line2: e.address_line2.clone(),
            phone: e.phone.clone(),
            website: e.website.clone(),
            opening_hours: e.opening_hours.clone(),
            wikidata_id: e.wikidata_id.clone(),
            distance_meters: e.distance_meters,
        }
    }
}

/// Weights are `distance ^ EXPONENT`, which favours farther pubs so a small
/// pool doesn't repeat itself. At 0.5 a pub at 400m is about twice as likely
/// as one at 100m. 0 is uniform, 1 linear, 2 quadratic.
const DISTANCE_EXPONENT: f64 = 0.5;

/// Floor so a pub at distance 0 can still be picked.
const MIN_DISTANCE_FLOOR: f64 = 10.0;

/// Per-element weights. `None` when fewer than two elements have distances.
fn distance_weights(elements: &[Element]) -> Option<Vec<f64>> {
    let weights: Vec<f64> = elements
        .iter()
        .map(|e| {
            e.distance_meters
                .unwrap_or(MIN_DISTANCE_FLOOR)
                .max(MIN_DISTANCE_FLOOR)
                .powf(DISTANCE_EXPONENT)
        })
        .collect();

    let have_distances = elements
        .iter()
        .filter(|e| e.distance_meters.is_some())
        .count();
    if have_distances >= 2 {
        Some(weights)
    } else {
        None
    }
}

/// Pick `n` distinct elements by weight, or shuffle uniformly without
/// distances.
fn weighted_sample(elements: &mut Vec<Element>, n: usize, rng: &mut impl Rng) -> Vec<Element> {
    if elements.len() <= n {
        elements.shuffle(rng);
        return std::mem::take(elements);
    }

    let weights = distance_weights(elements);

    match weights {
        Some(mut w) => {
            let mut selected = Vec::with_capacity(n);

            for _ in 0..n {
                let dist = match WeightedIndex::new(&w) {
                    Ok(d) => d,
                    Err(_) => {
                        // Weights went degenerate; finish uniformly.
                        elements.shuffle(rng);
                        selected.extend(elements.drain(..n - selected.len()));
                        break;
                    }
                };

                let idx = dist.sample(rng);
                selected.push(elements.swap_remove(idx));
                w.swap_remove(idx);
            }

            debug!(
                strategy = "distance_weighted",
                exponent = DISTANCE_EXPONENT,
                "selection_strategy"
            );
            selected
        }
        None => {
            debug!(strategy = "uniform", "selection_strategy");
            elements.shuffle(rng);
            elements.drain(..n).collect()
        }
    }
}

/// Pick a single element using distance-weighted sampling.
fn weighted_choose<'a>(elements: &'a [Element], rng: &mut impl Rng) -> Option<&'a Element> {
    if elements.is_empty() {
        return None;
    }

    match distance_weights(elements) {
        Some(w) => {
            let dist = WeightedIndex::new(&w).ok()?;
            let idx = dist.sample(rng);
            debug!(
                strategy = "distance_weighted",
                exponent = DISTANCE_EXPONENT,
                "selection_strategy"
            );
            Some(&elements[idx])
        }
        None => {
            debug!(strategy = "uniform", "selection_strategy");
            elements.choose(rng)
        }
    }
}

/// Great-circle distance in metres between two lat/lon points.
fn haversine_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();

    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().atan2((1.0 - a).sqrt())
}

/// Greedy nearest-neighbour ordering from `origin`. At our crawl lengths it
/// lands within a few percent of optimal and saves a routing call.
fn order_by_walking_proximity(
    origin_lat: f64,
    origin_lon: f64,
    mut pubs: Vec<Element>,
) -> Vec<Element> {
    let mut ordered = Vec::with_capacity(pubs.len());
    let mut current = (origin_lat, origin_lon);

    while !pubs.is_empty() {
        let (idx, _) = pubs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    i,
                    haversine_meters(
                        current.0,
                        current.1,
                        p.location.latitude,
                        p.location.longitude,
                    ),
                )
            })
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .expect("loop guard ensures non-empty");
        let next = pubs.swap_remove(idx);
        current = (next.location.latitude, next.location.longitude);
        ordered.push(next);
    }
    ordered
}

/// Optional filters applied to candidates before random selection.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlaceFilters {
    /// Only include venues with outdoor seating / a beer garden.
    pub beer_garden: bool,
}

#[allow(clippy::too_many_arguments)]
async fn fetch_candidates(
    config: &GeoapifyConfig,
    lat: f64,
    lon: f64,
    radius: i16,
    bearing: Option<f64>,
    mode: Mode,
    visited: &[String],
    filters: &PlaceFilters,
) -> Result<Vec<Element>, AppError> {
    let mut elements = crate::places_cache::get_places_in_radius(
        config, lat, lon, radius, bearing, mode,
    )
    .await
    .map_err(|e| {
        error!(error = %e, latitude = lat, longitude = lon, radius, mode = %mode, "geoapify_error");
        AppError::OsmError(e)
    })?;

    let before = elements.len();
    elements.retain(|e| e.place_id.as_ref().is_none_or(|id| !visited.contains(id)));
    let filtered_out = before - elements.len();

    if filtered_out > 0 {
        info!(
            total = before,
            filtered_out,
            remaining = elements.len(),
            "visited_filter_applied"
        );
    }

    if filters.beer_garden {
        let before = elements.len();
        elements.retain(|e| e.outdoor_seating);
        info!(
            total = before,
            remaining = elements.len(),
            filter = "beer_garden",
            "filter_applied"
        );
    }

    Ok(elements)
}

#[allow(clippy::too_many_arguments)]
pub async fn get_random_crawl(
    config: &GeoapifyConfig,
    length: i16,
    lat: f64,
    lon: f64,
    radius: i16,
    bearing: Option<f64>,
    mode: Mode,
    visited: &[String],
    filters: &PlaceFilters,
) -> Result<Vec<PubResponse>, AppError> {
    info!(
        latitude = lat, longitude = lon, radius, bearing,
        mode = %mode, num_pubs = length, visited_count = visited.len(),
        beer_garden = filters.beer_garden,
        "handling_request"
    );

    let mut candidates =
        fetch_candidates(config, lat, lon, radius, bearing, mode, visited, filters).await?;

    let requested = length as usize;
    let found = candidates.len();

    if found == 0 {
        warn!(mode = %mode, latitude = lat, longitude = lon, radius, "none_found");
        return Err(AppError::NoneFound { mode });
    }

    if found < requested {
        warn!(requested, found, mode = %mode, "not_enough_found");
        return Err(AppError::NotEnoughFound {
            requested: length,
            found,
            mode,
        });
    }

    let mut rng = rand::thread_rng();
    let selected = weighted_sample(&mut candidates, requested, &mut rng);
    let ordered = order_by_walking_proximity(lat, lon, selected);
    let responses: Vec<PubResponse> = ordered.into_iter().map(PubResponse::from).collect();

    let names: Vec<&str> = responses.iter().map(|r| r.name.as_str()).collect();
    info!(count = responses.len(), candidates = found, selected = ?names, "response_ok");

    Ok(responses)
}

/// Candidate pool for a vote-mode room: every venue in radius that has a
/// `place_id`, which votes and the visited filter both need. Unlike
/// [`get_random_crawl`] this keeps the full set so later rounds can resample.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_pool(
    config: &GeoapifyConfig,
    lat: f64,
    lon: f64,
    radius: i16,
    bearing: Option<f64>,
    mode: Mode,
    filters: &PlaceFilters,
) -> Result<Vec<PubResponse>, AppError> {
    let elements = fetch_candidates(config, lat, lon, radius, bearing, mode, &[], filters).await?;

    let before = elements.len();
    let pool: Vec<PubResponse> = elements
        .into_iter()
        .filter(|e| e.place_id.is_some())
        .map(PubResponse::from)
        .collect();

    let dropped = before - pool.len();
    if dropped > 0 {
        info!(
            total = before,
            dropped_no_place_id = dropped,
            kept = pool.len(),
            "pool_built"
        );
    } else {
        info!(kept = pool.len(), "pool_built");
    }

    if pool.is_empty() {
        warn!(mode = %mode, latitude = lat, longitude = lon, radius, "none_found");
        return Err(AppError::NoneFound { mode });
    }

    Ok(pool)
}

/// `weighted_sample` over an existing pool, skipping `exclude`, so a room
/// can pick each round's candidates without another upstream call. Returns
/// fewer than `n` if the pool runs out.
pub fn sample_weighted_pubs<'a>(
    pool: &'a [PubResponse],
    n: usize,
    exclude_place_ids: &std::collections::HashSet<String>,
    rng: &mut impl Rng,
) -> Vec<&'a PubResponse> {
    if n == 0 {
        return Vec::new();
    }

    // Sample by index so nothing is cloned until the caller keeps it.
    let mut available: Vec<usize> = pool
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match p.place_id.as_ref() {
            Some(pid) if !exclude_place_ids.contains(pid) => Some(i),
            _ => None,
        })
        .collect();

    if available.is_empty() {
        return Vec::new();
    }

    if available.len() <= n {
        available.shuffle(rng);
        return available.into_iter().map(|i| &pool[i]).collect();
    }

    let with_distance = available
        .iter()
        .filter(|&&i| pool[i].distance_meters.is_some())
        .count();

    let mut selected = Vec::with_capacity(n);

    if with_distance >= 2 {
        let mut weights: Vec<f64> = available
            .iter()
            .map(|&i| {
                pool[i]
                    .distance_meters
                    .unwrap_or(MIN_DISTANCE_FLOOR)
                    .max(MIN_DISTANCE_FLOOR)
                    .powf(DISTANCE_EXPONENT)
            })
            .collect();

        for _ in 0..n {
            let dist = match WeightedIndex::new(&weights) {
                Ok(d) => d,
                Err(_) => {
                    // Weights went degenerate; finish uniformly.
                    available.shuffle(rng);
                    selected.extend(
                        available
                            .drain(..(n - selected.len()).min(available.len()))
                            .map(|i| &pool[i]),
                    );
                    break;
                }
            };
            let idx_in_available = dist.sample(rng);
            let pool_idx = available.swap_remove(idx_in_available);
            weights.swap_remove(idx_in_available);
            selected.push(&pool[pool_idx]);
        }
        debug!(
            strategy = "distance_weighted",
            exponent = DISTANCE_EXPONENT,
            "selection_strategy"
        );
    } else {
        available.shuffle(rng);
        selected.extend(available.into_iter().take(n).map(|i| &pool[i]));
        debug!(strategy = "uniform", "selection_strategy");
    }

    selected
}

#[allow(clippy::too_many_arguments)]
pub async fn get_random_pub(
    config: &GeoapifyConfig,
    lat: f64,
    lon: f64,
    radius: i16,
    bearing: Option<f64>,
    mode: Mode,
    visited: &[String],
    filters: &PlaceFilters,
) -> Result<PubResponse, AppError> {
    info!(
        latitude = lat, longitude = lon, radius, bearing,
        mode = %mode, visited_count = visited.len(),
        beer_garden = filters.beer_garden,
        "handling_request"
    );

    let elements =
        fetch_candidates(config, lat, lon, radius, bearing, mode, visited, filters).await?;

    let mut rng = rand::thread_rng();
    let chosen = weighted_choose(&elements, &mut rng).ok_or_else(|| {
        warn!(mode = %mode, latitude = lat, longitude = lon, radius, "none_found");
        AppError::NoneFound { mode }
    })?;

    let response = PubResponse::from(chosen);
    info!(selected = %response.name, place_id = ?response.place_id, candidates = elements.len(), "response_ok");

    Ok(response)
}

#[derive(Debug)]
pub enum AppError {
    Validation(validator::ValidationErrors),
    NoneFound {
        mode: Mode,
    },
    OsmError(String),
    NotEnoughFound {
        requested: i16,
        found: usize,
        mode: Mode,
    },
    ParseError(String),
}

impl AppError {
    pub fn to_response(&self) -> (VercelStatusCode, String) {
        match self {
            // Generic on purpose: handlers log the full validator output
            // before mapping here, so field names stay server-side.
            AppError::Validation(_) => (
                VercelStatusCode::BAD_REQUEST,
                "Invalid request parameters.".to_string(),
            ),
            AppError::NoneFound { mode } => (
                VercelStatusCode::NOT_FOUND,
                format!("No {} found within the specified radius.", mode.label()),
            ),
            AppError::NotEnoughFound {
                requested,
                found,
                mode,
            } => (
                VercelStatusCode::NOT_FOUND,
                format!(
                    "Not enough {} found. Requested {}, but only {} found.",
                    mode.label(),
                    requested,
                    found
                ),
            ),
            AppError::OsmError(_) => (
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "An internal service error occurred.".to_string(),
            ),
            AppError::ParseError(e) => (
                VercelStatusCode::BAD_REQUEST,
                format!("Failed to parse request: {e}"),
            ),
        }
    }
}

impl From<validator::ValidationErrors> for AppError {
    fn from(err: validator::ValidationErrors) -> Self {
        AppError::Validation(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::places::{Element, Location, Names};
    use rand::SeedableRng;
    use std::collections::HashMap;

    fn mock_element(id: i64, distance: Option<f64>) -> Element {
        Element {
            elem_type: "node".to_owned(),
            id: id.to_string(),
            place_id: Some(format!("place_{id}")),
            location: Location {
                latitude: 51.5,
                longitude: -0.1,
            },
            names: Names {
                name: format!("Pub {id}"),
                alt_name: None,
                old_name: None,
            },
            address_line1: None,
            address_line2: None,
            distance_meters: distance,
            outdoor_seating: false,
            phone: None,
            website: None,
            opening_hours: None,
            wikidata_id: None,
            tags: HashMap::new(),
        }
    }

    #[test]
    fn distance_weights_returns_none_without_enough_distances() {
        let elements = vec![mock_element(1, None), mock_element(2, None)];
        assert!(distance_weights(&elements).is_none());

        let elements = vec![mock_element(1, Some(100.0)), mock_element(2, None)];
        assert!(distance_weights(&elements).is_none());
    }

    #[test]
    fn distance_weights_returns_some_with_two_or_more_distances() {
        let elements = vec![mock_element(1, Some(50.0)), mock_element(2, Some(200.0))];
        let w = distance_weights(&elements).expect("should produce weights");
        assert_eq!(w.len(), 2);
        assert!(w[1] > w[0], "farther pub should be weighted higher");
    }

    #[test]
    fn distance_weights_floors_at_minimum() {
        let elements = vec![mock_element(1, Some(0.0)), mock_element(2, Some(100.0))];
        let w = distance_weights(&elements).unwrap();
        assert!(w[0] > 0.0, "weight for distance=0 must be positive");
    }

    #[test]
    fn weighted_choose_returns_none_on_empty() {
        let elements: Vec<Element> = vec![];
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        assert!(weighted_choose(&elements, &mut rng).is_none());
    }

    #[test]
    fn weighted_choose_single_element() {
        let elements = vec![mock_element(1, Some(100.0))];
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let chosen = weighted_choose(&elements, &mut rng).unwrap();
        assert_eq!(chosen.id, "1");
    }

    #[test]
    fn weighted_choose_favours_farther_pubs() {
        let elements = vec![mock_element(1, Some(10.0)), mock_element(2, Some(10_000.0))];
        let mut near = 0u32;
        let mut far = 0u32;

        for seed in 0..10_000 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            match weighted_choose(&elements, &mut rng).unwrap().id.as_str() {
                "1" => near += 1,
                _ => far += 1,
            }
        }

        // weight(10) is ~3.16 against weight(10000) at 100, so far wins.
        assert!(
            far > near * 5,
            "far pub should be chosen much more often: near={near}, far={far}"
        );
    }

    #[test]
    fn weighted_choose_uniform_fallback_still_works() {
        let elements = vec![mock_element(1, None), mock_element(2, None)];
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        assert!(weighted_choose(&elements, &mut rng).is_some());
    }

    #[test]
    fn weighted_sample_returns_requested_count() {
        let mut elements = vec![
            mock_element(1, Some(50.0)),
            mock_element(2, Some(150.0)),
            mock_element(3, Some(300.0)),
            mock_element(4, Some(500.0)),
        ];
        let mut rng = rand::rngs::StdRng::seed_from_u64(99);
        let selected = weighted_sample(&mut elements, 2, &mut rng);
        assert_eq!(selected.len(), 2);

        let ids: std::collections::HashSet<&str> = selected.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "selected elements must be distinct");
    }

    #[test]
    fn weighted_sample_returns_all_when_n_equals_pool() {
        let mut elements = vec![mock_element(1, Some(50.0)), mock_element(2, Some(150.0))];
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let selected = weighted_sample(&mut elements, 2, &mut rng);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn weighted_sample_returns_all_when_n_exceeds_pool() {
        let mut elements = vec![mock_element(1, Some(50.0))];
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let selected = weighted_sample(&mut elements, 5, &mut rng);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn weighted_sample_without_distances_uses_uniform() {
        let mut elements = vec![
            mock_element(1, None),
            mock_element(2, None),
            mock_element(3, None),
        ];
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let selected = weighted_sample(&mut elements, 2, &mut rng);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn weighted_sample_does_not_repeat() {
        for seed in 0..200 {
            let mut elements = vec![
                mock_element(1, Some(10.0)),
                mock_element(2, Some(100.0)),
                mock_element(3, Some(500.0)),
                mock_element(4, Some(1000.0)),
            ];
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let selected = weighted_sample(&mut elements, 3, &mut rng);
            let ids: std::collections::HashSet<&str> =
                selected.iter().map(|e| e.id.as_str()).collect();
            assert_eq!(
                ids.len(),
                3,
                "seed {seed} produced duplicates: {:?}",
                selected.iter().map(|e| &e.id).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn pub_response_from_element() {
        let e = mock_element(42, Some(100.0));
        let resp = PubResponse::from(&e);
        assert_eq!(resp.name, "Pub 42");
        assert_eq!(resp.place_id.as_deref(), Some("place_42"));
    }

    #[test]
    fn validation_error_message_does_not_leak_field_names() {
        use validator::Validate;

        // Produces real ValidationErrors so the Display path runs.
        #[derive(Validate)]
        struct Probe {
            #[validate(range(min = -90.0, max = 90.0))]
            latitude: f64,
        }
        let raw = Probe { latitude: 999.0 }.validate().unwrap_err();
        let raw_display = format!("{raw}");
        assert!(raw_display.contains("latitude"));

        let (status, msg) = AppError::Validation(raw).to_response();
        assert_eq!(status.as_u16(), 400);
        assert!(!msg.contains("latitude"));
        assert!(!msg.contains("range"));
        assert_eq!(msg, "Invalid request parameters.");
    }

    #[test]
    fn osm_error_does_not_leak_upstream_details() {
        let (status, msg) =
            AppError::OsmError("geoapify returned 500: secret stuff".to_string()).to_response();
        assert_eq!(status.as_u16(), 500);
        assert!(!msg.contains("geoapify"));
        assert!(!msg.contains("secret"));
    }

    fn pub_at(id: i64, lat: f64, lon: f64) -> Element {
        let mut e = mock_element(id, None);
        e.location = Location {
            latitude: lat,
            longitude: lon,
        };
        e
    }

    #[test]
    fn haversine_returns_zero_for_same_point() {
        let d = haversine_meters(51.5, -0.1, 51.5, -0.1);
        assert!(d.abs() < 1e-6);
    }

    #[test]
    fn haversine_one_degree_lat_is_about_111km() {
        let d = haversine_meters(0.0, 0.0, 1.0, 0.0);
        assert!((d - 111_195.0).abs() < 100.0, "got {d}");
    }

    #[test]
    fn order_by_walking_proximity_picks_nearest_chain() {
        // Scrambled input; the greedy walk is A, B, C.
        let a = pub_at(1, 0.0, 0.001); // ~111m east
        let b = pub_at(2, 0.0, 0.002); // ~222m east
        let c = pub_at(3, 0.0, 0.003); // ~333m east
        let scrambled = vec![c.clone(), a.clone(), b.clone()];

        let ordered = order_by_walking_proximity(0.0, 0.0, scrambled);
        let ids: Vec<&str> = ordered.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["1", "2", "3"]);
    }

    #[test]
    fn order_by_walking_proximity_handles_single_pub() {
        let only = pub_at(1, 51.5, -0.1);
        let ordered = order_by_walking_proximity(0.0, 0.0, vec![only]);
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].id, "1");
    }

    #[test]
    fn order_by_walking_proximity_empty_input() {
        let ordered = order_by_walking_proximity(0.0, 0.0, vec![]);
        assert!(ordered.is_empty());
    }

    fn pub_response(id: &str, distance: Option<f64>) -> PubResponse {
        PubResponse {
            name: format!("Pub {id}"),
            latitude: 51.5,
            longitude: -0.1,
            place_id: Some(id.to_string()),
            address_line1: None,
            address_line2: None,
            phone: None,
            website: None,
            opening_hours: None,
            wikidata_id: None,
            distance_meters: distance,
        }
    }

    #[test]
    fn sample_weighted_pubs_excludes_visited() {
        let pool = vec![
            pub_response("a", Some(100.0)),
            pub_response("b", Some(200.0)),
            pub_response("c", Some(300.0)),
        ];
        let mut exclude = std::collections::HashSet::new();
        exclude.insert("b".to_string());
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let picks = sample_weighted_pubs(&pool, 5, &exclude, &mut rng);
        // 5 asked for, 2 available.
        assert_eq!(picks.len(), 2);
        let ids: std::collections::HashSet<&str> =
            picks.iter().filter_map(|p| p.place_id.as_deref()).collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("c"));
        assert!(!ids.contains("b"));
    }

    #[test]
    fn sample_weighted_pubs_picks_distinct() {
        let pool: Vec<PubResponse> = (0..6)
            .map(|i| pub_response(&format!("p{i}"), Some(100.0 * (i + 1) as f64)))
            .collect();
        let exclude = std::collections::HashSet::new();
        for seed in 0..50 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let picks = sample_weighted_pubs(&pool, 3, &exclude, &mut rng);
            assert_eq!(picks.len(), 3);
            let ids: std::collections::HashSet<&str> =
                picks.iter().filter_map(|p| p.place_id.as_deref()).collect();
            assert_eq!(ids.len(), 3, "seed {seed} produced duplicates");
        }
    }

    #[test]
    fn sample_weighted_pubs_empty_pool_returns_empty() {
        let pool: Vec<PubResponse> = vec![];
        let exclude = std::collections::HashSet::new();
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let picks = sample_weighted_pubs(&pool, 3, &exclude, &mut rng);
        assert!(picks.is_empty());
    }
}
