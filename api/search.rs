//! GET /api/search - place-name autocomplete, proxied to Nominatim.
//!
//! A proxy because their usage policy needs a real `User-Agent`, which a
//! browser can't set, and because we want our own per-IP limit on top of
//! theirs. It also keeps the client independent of the provider.

use http::Method;
use pubroulette::places;
use pubroulette::rate_limit;
use pubroulette::rooms;
use pubroulette::vercel::{Ctx, enforce_rate_limit, json_error, json_response};
use serde::Deserialize;
use tracing::{Instrument, error, info, warn};
use vercel_runtime::{Body, Error, Request, Response as VercelResponse, StatusCode, run};

/// The fields the frontend reads. Everything else is dropped rather than
/// echoing a pile of OSM metadata.
#[derive(Debug, Deserialize, serde::Serialize)]
struct NominatimResult {
    display_name: String,
    lat: String,
    lon: String,
    /// OSM tag key, such as "amenity" or "place".
    #[serde(default)]
    class: Option<String>,
    #[serde(default, rename = "type")]
    place_type: Option<String>,
    #[serde(default)]
    address: Option<serde_json::Value>,
}

/// Result filter. `None` forwards everything; the named kinds shape the
/// results for one caller each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The same venues the roulette itself returns. Matching on
    /// `(class, type)` rather than class alone keeps post boxes and ATMs out
    /// of a search for "the bear".
    Venue,
    /// Named areas: cities, postcodes, regions, islands. Used by the location
    /// picker, so "hebrides west" finds the islands and not a road.
    Area,
}

/// Pairs accepted under `kind=venue`. Must stay in sync with
/// `Mode::categories()`, or search offers venues the roulette can't return.
const VENUE_KINDS: &[(&str, &str)] = &[
    ("amenity", "pub"),
    ("amenity", "bar"),
    ("amenity", "biergarten"),
    ("amenity", "taproom"),
    ("amenity", "cafe"),
];

/// Classes accepted under `kind=area`: settlements, admin boundaries, named
/// natural features such as archipelagos, and land use like parks.
const AREA_CLASSES: &[&str] = &["place", "boundary", "natural", "landuse"];

impl Kind {
    /// Match against the kind's allow-list. A record missing either field is
    /// dropped rather than guessed at.
    fn matches(self, class: Option<&str>, place_type: Option<&str>) -> bool {
        match self {
            Kind::Venue => match (class, place_type) {
                (Some(c), Some(t)) => VENUE_KINDS.iter().any(|(vc, vt)| *vc == c && *vt == t),
                _ => false,
            },
            Kind::Area => match class {
                Some(c) => AREA_CLASSES.contains(&c),
                None => false,
            },
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(handler).await
}

async fn handler(req: Request) -> Result<VercelResponse<Body>, Error> {
    pubroulette::init_tracing();
    let mut ctx = Ctx::from_request(&req);
    let span = pubroulette::request_span("search", &ctx.request_id);

    async move {
        if req.method() == Method::OPTIONS {
            return Ok(VercelResponse::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::Empty)?);
        }
        if req.method() != Method::GET {
            return json_error(&ctx, StatusCode::METHOD_NOT_ALLOWED, "Use GET");
        }

        // Read-only endpoint, so no limiter beats a 500.
        match rooms::connect().await {
            Ok(mut conn) => {
                if let Err(resp) =
                    enforce_rate_limit(&mut conn, rate_limit::SEARCH, &req, &mut ctx).await
                {
                    return resp;
                }
            }
            Err(e) => warn!(error = ?e, "rate_limit_redis_unavailable"),
        }

        let raw_query = req.uri().query().unwrap_or("");
        let q = parse_query(raw_query);
        let q = match q {
            Some(q) => q,
            None => {
                return json_error(
                    &ctx,
                    StatusCode::BAD_REQUEST,
                    "Provide a 2-100 character `q` query parameter.",
                );
            }
        };
        let bias = parse_bias(raw_query);
        let kind = parse_kind(raw_query);

        match nominatim_search(&q, bias).await {
            Ok(results) => {
                let filtered: Vec<NominatimResult> = match kind {
                    Some(k) => results
                        .into_iter()
                        .filter(|r| k.matches(r.class.as_deref(), r.place_type.as_deref()))
                        .collect(),
                    None => results,
                };
                json_response(&ctx, StatusCode::OK, &filtered)
            }
            Err(e) => {
                error!(error = %e, "search_upstream_error");
                json_error(
                    &ctx,
                    StatusCode::BAD_GATEWAY,
                    "Search is temporarily unavailable.",
                )
            }
        }
    }
    .instrument(span)
    .await
}

/// Pull `q` out of the query string, bounded at both ends: the frontend only
/// searches from 2 characters, and long inputs shouldn't reach Nominatim.
fn parse_query(raw: &str) -> Option<String> {
    for pair in raw.split('&') {
        let mut split = pair.splitn(2, '=');
        let key = split.next()?;
        if key != "q" {
            continue;
        }
        let raw_val = split.next().unwrap_or("");
        let decoded = url_decode(raw_val);
        let trimmed = decoded.trim();
        if (2..=100).contains(&trimmed.chars().count()) {
            return Some(trimmed.to_owned());
        }
    }
    None
}

/// Optional proximity bias. Nominatim has no such parameter, so
/// [`nominatim_search`] turns it into a viewbox with `bounded=0`, which
/// demotes rather than excludes everything outside.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bias {
    lat: f64,
    lon: f64,
}

/// Read the optional `kind` parameter, case-insensitively. Unknown or missing
/// means no filtering.
fn parse_kind(raw: &str) -> Option<Kind> {
    for pair in raw.split('&') {
        let mut split = pair.splitn(2, '=');
        let key = split.next()?;
        if key != "kind" {
            continue;
        }
        let val = url_decode(split.next().unwrap_or(""))
            .trim()
            .to_ascii_lowercase();
        return match val.as_str() {
            "venue" => Some(Kind::Venue),
            "area" => Some(Kind::Area),
            _ => None,
        };
    }
    None
}

fn parse_bias(raw: &str) -> Option<Bias> {
    let mut lat: Option<f64> = None;
    let mut lon: Option<f64> = None;
    for pair in raw.split('&') {
        let mut split = pair.splitn(2, '=');
        let key = split.next()?;
        let val = split.next().unwrap_or("");
        match key {
            "lat" => lat = url_decode(val).trim().parse().ok(),
            "lon" => lon = url_decode(val).trim().parse().ok(),
            _ => {}
        }
    }
    let (lat, lon) = (lat?, lon?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some(Bias { lat, lon })
}

/// Enough form decoding for the `q` param, without a dependency.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn nominatim_search(q: &str, bias: Option<Bias>) -> Result<Vec<NominatimResult>, String> {
    let base = std::env::var("NOMINATIM_BASE_URL")
        .unwrap_or_else(|_| "https://nominatim.openstreetmap.org/search".to_owned());

    let mut params: Vec<(&str, String)> = vec![
        ("q", q.to_owned()),
        ("format", "json".to_owned()),
        ("limit", "5".to_owned()),
        ("addressdetails", "1".to_owned()),
    ];
    if let Some(b) = bias {
        // A box roughly 100km wide, sent with `bounded=0` so outside results
        // rank lower but still appear. A degree of longitude shrinks with
        // cos(lat).
        let half_lat_deg: f64 = 0.45; // ~50km north/south
        let cos_lat = b.lat.to_radians().cos().abs().max(0.05);
        let half_lon_deg: f64 = 0.45 / cos_lat;
        let left = b.lon - half_lon_deg;
        let right = b.lon + half_lon_deg;
        let top = (b.lat + half_lat_deg).min(90.0);
        let bottom = (b.lat - half_lat_deg).max(-90.0);
        params.push(("viewbox", format!("{left},{top},{right},{bottom}")));
        params.push(("bounded", "0".to_owned()));
    }

    // The shared client already sets the `User-Agent` their policy requires.
    let resp = places::http_client()
        .get(base)
        .query(&params)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "nominatim_timeout".to_owned()
            } else {
                format!("nominatim_request_error: {e}")
            }
        })?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("nominatim_status_{status}: {body}"));
    }

    let results: Vec<NominatimResult> = resp
        .json()
        .await
        .map_err(|e| format!("nominatim_parse_error: {e}"))?;

    info!(count = results.len(), "search_ok");
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_accepts_min_length() {
        assert_eq!(parse_query("q=ab"), Some("ab".to_owned()));
    }

    #[test]
    fn parse_query_rejects_too_short() {
        assert_eq!(parse_query("q=a"), None);
        assert_eq!(parse_query("q="), None);
        assert_eq!(parse_query(""), None);
    }

    #[test]
    fn parse_query_rejects_too_long() {
        let long = "x".repeat(101);
        assert_eq!(parse_query(&format!("q={long}")), None);
    }

    #[test]
    fn parse_query_decodes_percent_encoding() {
        assert_eq!(
            parse_query("q=Edinburgh%2C%20Scotland"),
            Some("Edinburgh, Scotland".to_owned())
        );
    }

    #[test]
    fn parse_query_decodes_plus_as_space() {
        assert_eq!(parse_query("q=hello+world"), Some("hello world".to_owned()));
    }

    #[test]
    fn parse_query_ignores_other_params() {
        assert_eq!(
            parse_query("limit=5&q=cafe&format=json"),
            Some("cafe".to_owned())
        );
    }

    #[test]
    fn parse_bias_returns_none_when_missing() {
        assert_eq!(parse_bias("q=cafe"), None);
        assert_eq!(parse_bias("q=cafe&lat=55.95"), None);
        assert_eq!(parse_bias("q=cafe&lon=-3.19"), None);
    }

    #[test]
    fn parse_bias_parses_valid_coords() {
        assert_eq!(
            parse_bias("q=cafe&lat=55.95&lon=-3.19"),
            Some(Bias {
                lat: 55.95,
                lon: -3.19
            }),
        );
    }

    #[test]
    fn parse_bias_rejects_out_of_range() {
        assert_eq!(parse_bias("lat=91&lon=0"), None);
        assert_eq!(parse_bias("lat=0&lon=181"), None);
    }

    #[test]
    fn parse_bias_rejects_garbage() {
        assert_eq!(parse_bias("lat=abc&lon=0"), None);
    }

    #[test]
    fn parse_kind_recognises_venue_and_area() {
        assert_eq!(parse_kind("q=foo&kind=venue"), Some(Kind::Venue));
        assert_eq!(parse_kind("q=foo&kind=area"), Some(Kind::Area));
    }

    #[test]
    fn parse_kind_is_case_insensitive() {
        assert_eq!(parse_kind("kind=Venue"), Some(Kind::Venue));
        assert_eq!(parse_kind("kind=AREA"), Some(Kind::Area));
    }

    #[test]
    fn parse_kind_returns_none_for_unknown_or_missing() {
        assert_eq!(parse_kind("q=foo"), None);
        assert_eq!(parse_kind("kind=wat"), None);
        assert_eq!(parse_kind(""), None);
    }

    #[test]
    fn kind_venue_matches_pub_and_bar_but_not_other_amenities() {
        // Class alone would forward ATMs and post boxes, which share
        // `class=amenity` with pubs.
        for (c, t) in VENUE_KINDS {
            assert!(
                Kind::Venue.matches(Some(c), Some(t)),
                "venue should match ({c}, {t})"
            );
        }
        assert!(!Kind::Venue.matches(Some("amenity"), Some("post_box")));
        assert!(!Kind::Venue.matches(Some("amenity"), Some("atm")));
        assert!(!Kind::Venue.matches(Some("amenity"), Some("school")));
        assert!(!Kind::Venue.matches(Some("highway"), Some("residential")));
        assert!(!Kind::Venue.matches(Some("place"), Some("archipelago")));
        assert!(!Kind::Venue.matches(Some("boundary"), Some("administrative")));
    }

    #[test]
    fn kind_venue_pub_categories_mirror_places_module() {
        // A new category in `Mode::Pubs` must be added here too, or search
        // stops finding things the roulette returns.
        let pub_categories = pubroulette::places::Mode::Pubs.categories();
        let cafe_categories = pubroulette::places::Mode::NonAlcohol.categories();
        for cat in pub_categories.split(',').chain(cafe_categories.split(',')) {
            let osm_type = cat.trim_start_matches("catering.");
            assert!(
                VENUE_KINDS
                    .iter()
                    .any(|(c, t)| *c == "amenity" && *t == osm_type),
                "pub roulette category `{cat}` has no matching VENUE_KIND entry"
            );
        }
    }

    #[test]
    fn kind_area_matches_place_and_natural_but_not_highway_or_amenity() {
        // "hebrides west" used to return roads in LA, so `kind=area` must
        // drop `class=highway`.
        assert!(Kind::Area.matches(Some("place"), Some("archipelago")));
        assert!(Kind::Area.matches(Some("place"), Some("city")));
        assert!(Kind::Area.matches(Some("place"), Some("postcode")));
        assert!(Kind::Area.matches(Some("boundary"), Some("administrative")));
        assert!(Kind::Area.matches(Some("natural"), Some("peak")));
        assert!(Kind::Area.matches(Some("landuse"), Some("forest")));
        assert!(!Kind::Area.matches(Some("highway"), Some("residential")));
        // A single pub isn't an area; that's what `kind=venue` is for.
        assert!(!Kind::Area.matches(Some("amenity"), Some("pub")));
    }

    #[test]
    fn kind_filter_drops_results_with_no_class_or_type() {
        // An untagged hit is dropped under any explicit kind, but still
        // appears when no kind is given.
        assert!(!Kind::Venue.matches(None, None));
        assert!(!Kind::Venue.matches(Some("amenity"), None));
        assert!(!Kind::Venue.matches(None, Some("pub")));
        assert!(!Kind::Area.matches(None, None));
    }
}
