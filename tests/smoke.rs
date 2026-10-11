//! Smoke tests against the real Geoapify API.
//!
//! `#[ignore]`d so `cargo test` doesn't burn quota. Needs `GEOAPIFY_API_KEY`
//! in the environment or `.env`, then:
//!
//! ```sh
//! cargo test --test smoke -- --ignored
//! ```

use pubroulette::places::{GeoapifyConfig, Mode};
use pubroulette::roulette::{self, AppError, PlaceFilters};

fn config() -> GeoapifyConfig {
    let _ = dotenvy::dotenv();
    GeoapifyConfig::from_env().expect("GEOAPIFY_API_KEY must be set to run smoke tests")
}

/// A location to test against, with a human label for diagnostics.
struct Spot {
    label: &'static str,
    lat: f64,
    lon: f64,
}

const SPOTS: &[Spot] = &[
    // Dense cities, which should almost always have pubs.
    Spot {
        label: "London (Soho)",
        lat: 51.5134,
        lon: -0.1372,
    },
    Spot {
        label: "Edinburgh (Old Town)",
        lat: 55.9497,
        lon: -3.1905,
    },
    Spot {
        label: "Dublin (Temple Bar)",
        lat: 53.3457,
        lon: -6.2643,
    },
    Spot {
        label: "New York (East Village)",
        lat: 40.7265,
        lon: -73.9815,
    },
    Spot {
        label: "Melbourne (CBD)",
        lat: -37.8136,
        lon: 144.9631,
    },
    Spot {
        label: "Tokyo (Shibuya)",
        lat: 35.6580,
        lon: 139.7016,
    },
    Spot {
        label: "Berlin (Kreuzberg)",
        lat: 52.4988,
        lon: 13.4185,
    },
    Spot {
        label: "Cape Town (Long St)",
        lat: -33.9221,
        lon: 18.4171,
    },
    // Sparse places, where finding nothing is a valid result.
    Spot {
        label: "Sahara Desert",
        lat: 23.4162,
        lon: 2.2137,
    },
    Spot {
        label: "Mid-Atlantic",
        lat: 35.0,
        lon: -40.0,
    },
    Spot {
        label: "Rural Iceland",
        lat: 65.0,
        lon: -18.5,
    },
    Spot {
        label: "Antarctica (McMurdo)",
        lat: -77.8460,
        lon: 166.6686,
    },
    // Poles and the dateline, for the geometry edge cases.
    Spot {
        label: "Svalbard",
        lat: 78.2232,
        lon: 15.6267,
    },
    Spot {
        label: "Fiji (near dateline)",
        lat: -17.7765,
        lon: 177.9631,
    },
    Spot {
        label: "Near North Pole",
        lat: 89.0,
        lon: 0.0,
    },
    Spot {
        label: "Near South Pole",
        lat: -89.0,
        lon: 0.0,
    },
    Spot {
        label: "Buenos Aires",
        lat: -34.6037,
        lon: -58.3816,
    },
    Spot {
        label: "São Paulo",
        lat: -23.5505,
        lon: -46.6333,
    },
    Spot {
        label: "Sydney (Surry Hills)",
        lat: -33.8830,
        lon: 151.2106,
    },
];

const RADII: &[i16] = &[50, 200, 500, 1000];
const BEARINGS: &[Option<f64>] = &[
    None,
    Some(0.0),
    Some(90.0),
    Some(180.0),
    Some(270.0),
    Some(359.0),
];
const MODES: &[Mode] = &[Mode::Pubs, Mode::NonAlcohol];

/// Accept only the errors a known-good input can legitimately produce.
fn assert_acceptable_play_error(
    err: &AppError,
    spot: &Spot,
    radius: i16,
    bearing: Option<f64>,
    mode: Mode,
) {
    match err {
        AppError::NoneFound { .. } => {} // totally fine
        other => {
            let (status, msg) = other.to_response();
            assert!(
                !status.is_server_error(),
                "Unexpected server error for play at {} (radius={radius}, bearing={bearing:?}, mode={mode}): {status} {msg}",
                spot.label,
            );
            assert!(
                !matches!(other, AppError::Validation(_) | AppError::ParseError(_)),
                "Unexpected client error for play at {} (radius={radius}, bearing={bearing:?}, mode={mode}): {status} {msg}",
                spot.label,
            );
        }
    }
}

/// As above, for crawls.
fn assert_acceptable_crawl_error(
    err: &AppError,
    spot: &Spot,
    radius: i16,
    bearing: Option<f64>,
    mode: Mode,
    num_pubs: i16,
) {
    match err {
        AppError::NoneFound { .. } | AppError::NotEnoughFound { .. } => {} // expected in sparse areas
        other => {
            let (status, msg) = other.to_response();
            assert!(
                !status.is_server_error(),
                "Unexpected server error for crawl at {} (radius={radius}, bearing={bearing:?}, mode={mode}, n={num_pubs}): {status} {msg}",
                spot.label,
            );
            assert!(
                !matches!(other, AppError::Validation(_) | AppError::ParseError(_)),
                "Unexpected client error for crawl at {} (radius={radius}, bearing={bearing:?}, mode={mode}, n={num_pubs}): {status} {msg}",
                spot.label,
            );
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_play_across_locations_and_radii() {
    let config = config();

    for spot in SPOTS {
        for &radius in RADII {
            let result = roulette::get_random_pub(
                &config,
                spot.lat,
                spot.lon,
                radius,
                None,
                Mode::Pubs,
                &[],
                &PlaceFilters::default(),
            )
            .await;

            match &result {
                Ok(pub_resp) => {
                    assert!(!pub_resp.name.is_empty(), "Empty name at {}", spot.label);
                    eprintln!("  ok {} r={radius}: {}", spot.label, pub_resp.name);
                }
                Err(e) => {
                    assert_acceptable_play_error(e, spot, radius, None, Mode::Pubs);
                    eprintln!("  -- {} r={radius}: none found (ok)", spot.label);
                }
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_play_with_bearings() {
    let config = config();

    // Bearings only matter where there are results to filter.
    let dense_spots = &SPOTS[..8];

    for spot in dense_spots {
        for &bearing in BEARINGS {
            let result = roulette::get_random_pub(
                &config,
                spot.lat,
                spot.lon,
                500,
                bearing,
                Mode::Pubs,
                &[],
                &PlaceFilters::default(),
            )
            .await;

            match &result {
                Ok(pub_resp) => {
                    assert!(!pub_resp.name.is_empty());
                    if let Some(b) = bearing {
                        eprintln!("  ok {} bearing={b}deg: {}", spot.label, pub_resp.name);
                    }
                }
                Err(e) => {
                    assert_acceptable_play_error(e, spot, 500, bearing, Mode::Pubs);
                }
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_play_both_modes() {
    let config = config();

    for spot in SPOTS {
        for &mode in MODES {
            let result = roulette::get_random_pub(
                &config,
                spot.lat,
                spot.lon,
                500,
                None,
                mode,
                &[],
                &PlaceFilters::default(),
            )
            .await;

            match &result {
                Ok(pub_resp) => {
                    assert!(!pub_resp.name.is_empty());
                    eprintln!("  ok {} mode={mode}: {}", spot.label, pub_resp.name);
                }
                Err(e) => {
                    assert_acceptable_play_error(e, spot, 500, None, mode);
                }
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_play_with_visited_filter() {
    let config = config();

    let spot = &SPOTS[0]; // London Soho

    let first = roulette::get_random_pub(
        &config,
        spot.lat,
        spot.lon,
        500,
        None,
        Mode::Pubs,
        &[],
        &PlaceFilters::default(),
    )
    .await;

    if let Ok(first_pub) = first
        && let Some(ref place_id) = first_pub.place_id
    {
        let visited = vec![place_id.clone()];
        let second = roulette::get_random_pub(
            &config,
            spot.lat,
            spot.lon,
            500,
            None,
            Mode::Pubs,
            &visited,
            &PlaceFilters::default(),
        )
        .await;

        match second {
            Ok(second_pub) => {
                // The name may repeat, but the place_id must not.
                assert_ne!(
                    second_pub.place_id.as_deref(),
                    Some(place_id.as_str()),
                    "Visited pub should have been excluded"
                );
                eprintln!(
                    "  ok visited filter works: got {} after excluding {}",
                    second_pub.name, first_pub.name
                );
            }
            Err(e) => {
                assert_acceptable_play_error(&e, spot, 500, None, Mode::Pubs);
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_crawl_across_locations() {
    let config = config();

    for spot in SPOTS {
        let result = roulette::get_random_crawl(
            &config,
            3,
            spot.lat,
            spot.lon,
            500,
            None,
            Mode::Pubs,
            &[],
            &PlaceFilters::default(),
        )
        .await;

        match &result {
            Ok(pubs) => {
                assert!(!pubs.is_empty());
                assert!(pubs.len() <= 3);
                let ids: std::collections::HashSet<_> =
                    pubs.iter().filter_map(|p| p.place_id.as_deref()).collect();
                assert_eq!(
                    ids.len(),
                    pubs.len(),
                    "Crawl returned duplicate place_ids at {}",
                    spot.label
                );

                let names: Vec<&str> = pubs.iter().map(|p| p.name.as_str()).collect();
                eprintln!("  ok {} crawl: {}", spot.label, names.join(", "));
            }
            Err(e) => {
                assert_acceptable_crawl_error(e, spot, 500, None, Mode::Pubs, 3);
                eprintln!("  -- {} crawl: {:?} (ok)", spot.label, e);
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_crawl_various_sizes() {
    let config = config();
    let spot = &SPOTS[0]; // London

    for num_pubs in [1, 2, 3, 4, 5] {
        let result = roulette::get_random_crawl(
            &config,
            num_pubs,
            spot.lat,
            spot.lon,
            1000,
            None,
            Mode::Pubs,
            &[],
            &PlaceFilters::default(),
        )
        .await;

        match &result {
            Ok(pubs) => {
                assert_eq!(
                    pubs.len(),
                    num_pubs as usize,
                    "Should get exactly {num_pubs} pubs"
                );
                eprintln!("  ok crawl n={num_pubs}: got {} pubs", pubs.len());
            }
            Err(e) => {
                assert_acceptable_crawl_error(e, spot, 1000, None, Mode::Pubs, num_pubs);
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_crawl_with_bearing_and_mode() {
    let config = config();
    let spot = &SPOTS[1]; // Edinburgh

    for &mode in MODES {
        for &bearing in &[None, Some(45.0), Some(225.0)] {
            let result = roulette::get_random_crawl(
                &config,
                2,
                spot.lat,
                spot.lon,
                500,
                bearing,
                mode,
                &[],
                &PlaceFilters::default(),
            )
            .await;

            match &result {
                Ok(pubs) => {
                    assert_eq!(pubs.len(), 2);
                    for p in pubs {
                        assert!(!p.name.is_empty());
                    }
                }
                Err(e) => {
                    assert_acceptable_crawl_error(e, spot, 500, bearing, mode, 2);
                }
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_minimum_radius() {
    let config = config();

    for spot in &SPOTS[..5] {
        let result = roulette::get_random_pub(
            &config,
            spot.lat,
            spot.lon,
            1,
            None,
            Mode::Pubs,
            &[],
            &PlaceFilters::default(),
        )
        .await;

        match result {
            Ok(p) => eprintln!("  ok {} r=1: {}", spot.label, p.name),
            Err(e) => {
                assert_acceptable_play_error(&e, spot, 1, None, Mode::Pubs);
                eprintln!("  -- {} r=1: none found (ok)", spot.label);
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_maximum_radius() {
    let config = config();

    let spot = &SPOTS[0]; // London
    let result = roulette::get_random_pub(
        &config,
        spot.lat,
        spot.lon,
        1000,
        None,
        Mode::Pubs,
        &[],
        &PlaceFilters::default(),
    )
    .await;

    match result {
        Ok(p) => {
            assert!(!p.name.is_empty());
            eprintln!("  ok {} r=1000: {}", spot.label, p.name);
        }
        Err(e) => {
            assert_acceptable_play_error(&e, spot, 1000, None, Mode::Pubs);
        }
    }
}

#[tokio::test]
#[ignore]
async fn smoke_bearing_boundary_values() {
    let config = config();
    let spot = &SPOTS[0]; // London

    for bearing in [0.0, 1.0, 179.0, 180.0, 359.0] {
        let result = roulette::get_random_pub(
            &config,
            spot.lat,
            spot.lon,
            500,
            Some(bearing),
            Mode::Pubs,
            &[],
            &PlaceFilters::default(),
        )
        .await;

        match result {
            Ok(p) => eprintln!("  ok bearing={bearing}deg: {}", p.name),
            Err(e) => {
                assert_acceptable_play_error(&e, spot, 500, Some(bearing), Mode::Pubs);
            }
        }
    }
}
