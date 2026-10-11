//! Vote-mode round engine: how a room picks its next pub.
//!
//! `rooms.rs` opens a round with N candidates, members submit approval votes,
//! then [`resolve`] picks the winner. Every function here is pure and takes its
//! rng explicitly, so `rooms.rs` keeps sole charge of state.
//!
//! A device approves any number of candidates. Each candidate's score is the
//! number of distinct devices that approved it, and the highest score wins.
//! Ties, including the all-abstained case, are broken uniformly at random and
//! reported with `tiebroken: true`.

use crate::roulette::PubResponse;
use chrono::{DateTime, SecondsFormat, Utc};
use rand::Rng;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Serialize as RFC3339 with fixed millisecond precision.
///
/// The Lua expiry checks compare these strings lexically. Chrono's default
/// strips trailing zeros, which misorders `T12:00:00Z` against
/// `T12:00:00.123Z` because `.` sorts before `Z`. Fixed precision makes the
/// comparison total.
pub mod rfc3339_millis {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&dt.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let s = String::deserialize(d)?;
        DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}

/// [`rfc3339_millis`] for hand-built strings, such as `now` for a Lua script.
pub fn now_rfc3339_millis(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Candidates per round. Enough to feel like a vote, few enough for a phone
/// screen.
pub const DEFAULT_CANDIDATES_PER_ROUND: u8 = 3;

/// Voting window per round. Long enough to notice, short enough that one
/// stalled member can't stall the group.
pub const DEFAULT_ROUND_SECONDS: u32 = 60;

/// Reshuffles a host gets per crawl. A safety valve, not a free reroll.
pub const MAX_SKIPS_PER_CRAWL: u8 = 2;

pub const MIN_CANDIDATES_PER_ROUND: u8 = 2;
pub const MAX_CANDIDATES_PER_ROUND: u8 = 5;
/// Upper bound on `round_seconds`; lower bound is 0 (manual close only).
pub const MAX_ROUND_SECONDS: u32 = 300;

/// How a room picks its pubs.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DecisionMode {
    /// Server picks every pub up front and the host advances.
    #[default]
    Dictator,
    /// Per-stop approval vote with a roulette tiebreak.
    Vote,
}

/// One voting round, held in `Room.current_round` and cleared when the room
/// finishes.
///
/// `votes` stays `None` until the first vote because lua-cjson can't tell an
/// empty table from an empty array on re-encode, and `None` round-trips
/// cleanly through every script that loads the room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Round {
    /// The `pubs[]` slot this round fills, so clients can show "Pub 2 of 5".
    pub index: u8,
    pub candidates: Vec<PubResponse>,
    #[serde(with = "rfc3339_millis")]
    pub opened_at: DateTime<Utc>,
    /// Deadline after which the round may be resolved. Millisecond precision
    /// so the Lua lex-compare against [`now_rfc3339_millis`] is total.
    #[serde(with = "rfc3339_millis")]
    pub expires_at: DateTime<Utc>,
    /// device_id to approved place_ids, treated as a set. `None` until the
    /// first vote arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub votes: Option<BTreeMap<String, Vec<String>>>,
    /// Set when the round closes, and kept until the next round opens so
    /// clients can show what the group picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedRound>,
}

impl Round {
    /// True while members can still cast votes.
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        self.resolved.is_none() && now < self.expires_at
    }

    /// True when the server may resolve this round.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.resolved.is_none() && now >= self.expires_at
    }
}

/// One candidate's score, ordered to match `Round.candidates`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateTally {
    pub place_id: String,
    pub count: u32,
}

/// The winner plus the tally behind it. `tiebroken` marks a random pick, from
/// either a real tie or no votes at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedRound {
    pub winner: PubResponse,
    pub tally: Vec<CandidateTally>,
    pub tiebroken: bool,
    pub resolved_at: DateTime<Utc>,
}

/// Score each candidate, in `round.candidates` order. A device's duplicate
/// approvals count once.
pub fn tally(round: &Round) -> Vec<CandidateTally> {
    // Seed every candidate at zero so order and length are deterministic.
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for c in &round.candidates {
        if let Some(pid) = c.place_id.as_deref() {
            counts.entry(pid).or_insert(0);
        }
    }

    if let Some(votes) = &round.votes {
        for approvals in votes.values() {
            // A client could submit ["x","x"]; count it once.
            let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            for pid in approvals {
                if seen.insert(pid.as_str())
                    && let Some(c) = counts.get_mut(pid.as_str())
                {
                    *c += 1;
                }
            }
        }
    }

    round
        .candidates
        .iter()
        .filter_map(|c| {
            let pid = c.place_id.as_deref()?;
            Some(CandidateTally {
                place_id: pid.to_string(),
                count: *counts.get(pid).unwrap_or(&0),
            })
        })
        .collect()
}

/// Pick the round's winner. Returns the resolution with the round so the
/// caller can commit it.
pub fn resolve<R: Rng>(round: &Round, now: DateTime<Utc>, rng: &mut R) -> Option<ResolvedRound> {
    if round.candidates.is_empty() {
        return None;
    }

    let tally = tally(round);
    let max_count = tally.iter().map(|t| t.count).max().unwrap_or(0);

    // With no votes every candidate ties at zero, so the same path handles it.
    let winners: Vec<&PubResponse> = round
        .candidates
        .iter()
        .filter(|c| {
            c.place_id
                .as_deref()
                .and_then(|pid| tally.iter().find(|t| t.place_id == pid))
                .map(|t| t.count == max_count)
                .unwrap_or(false)
        })
        .collect();

    let (winner, tiebroken) = if winners.len() == 1 && max_count > 0 {
        (winners[0], false)
    } else {
        // `winners` can't be empty: candidates are non-empty and all are
        // seeded into the tally.
        let pick = winners.choose(rng).copied()?;
        (pick, true)
    };

    Some(ResolvedRound {
        winner: winner.clone(),
        tally,
        tiebroken,
        resolved_at: now,
    })
}

/// Clamp `candidates_per_round` to the allowed range, defaulting when `None`.
/// Shared so settings validation and round construction agree.
pub fn effective_candidates_per_round(raw: Option<u8>) -> u8 {
    raw.unwrap_or(DEFAULT_CANDIDATES_PER_ROUND)
        .clamp(MIN_CANDIDATES_PER_ROUND, MAX_CANDIDATES_PER_ROUND)
}

/// Resolve `round_seconds`, keeping zero, which means the host closes manually.
pub fn effective_round_seconds(raw: Option<u32>) -> u32 {
    let value = raw.unwrap_or(DEFAULT_ROUND_SECONDS);
    value.min(MAX_ROUND_SECONDS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn pub_at(place_id: &str, distance: Option<f64>) -> PubResponse {
        PubResponse {
            name: format!("Pub {place_id}"),
            latitude: 51.5,
            longitude: -0.1,
            place_id: Some(place_id.to_string()),
            address_line1: None,
            address_line2: None,
            phone: None,
            website: None,
            opening_hours: None,
            wikidata_id: None,
            distance_meters: distance,
        }
    }

    fn round_with_votes(votes: Vec<(&str, Vec<&str>)>) -> Round {
        let candidates = vec![
            pub_at("a", Some(100.0)),
            pub_at("b", Some(200.0)),
            pub_at("c", Some(300.0)),
        ];
        let votes = if votes.is_empty() {
            None
        } else {
            Some(
                votes
                    .into_iter()
                    .map(|(dev, approvals)| {
                        (
                            dev.to_string(),
                            approvals.into_iter().map(|s| s.to_string()).collect(),
                        )
                    })
                    .collect(),
            )
        };
        let now = Utc::now();
        Round {
            index: 0,
            candidates,
            opened_at: now,
            expires_at: now + chrono::Duration::seconds(60),
            votes,
            resolved: None,
        }
    }

    #[test]
    fn tally_counts_distinct_devices() {
        let round = round_with_votes(vec![
            ("dev1", vec!["a", "b"]),
            ("dev2", vec!["a"]),
            ("dev3", vec!["b", "c"]),
        ]);
        let t = tally(&round);
        let by_id: BTreeMap<&str, u32> = t.iter().map(|c| (c.place_id.as_str(), c.count)).collect();
        assert_eq!(by_id.get("a"), Some(&2));
        assert_eq!(by_id.get("b"), Some(&2));
        assert_eq!(by_id.get("c"), Some(&1));
    }

    #[test]
    fn tally_dedups_within_device_approvals() {
        // ["a","a"] is one approval.
        let round = round_with_votes(vec![("dev1", vec!["a", "a", "a"])]);
        let t = tally(&round);
        let a = t.iter().find(|c| c.place_id == "a").unwrap();
        assert_eq!(a.count, 1);
    }

    #[test]
    fn tally_ignores_unknown_place_ids() {
        let round = round_with_votes(vec![("dev1", vec!["zzz"])]);
        let t = tally(&round);
        assert!(t.iter().all(|c| c.count == 0));
    }

    #[test]
    fn tally_empty_votes_returns_all_zero() {
        let round = round_with_votes(vec![]);
        let t = tally(&round);
        assert_eq!(t.len(), 3);
        assert!(t.iter().all(|c| c.count == 0));
    }

    #[test]
    fn resolve_clear_winner_no_tiebreak() {
        let round = round_with_votes(vec![
            ("dev1", vec!["a"]),
            ("dev2", vec!["a"]),
            ("dev3", vec!["b"]),
        ]);
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let resolved = resolve(&round, Utc::now(), &mut rng).expect("must resolve");
        assert_eq!(resolved.winner.place_id.as_deref(), Some("a"));
        assert!(!resolved.tiebroken);
    }

    #[test]
    fn resolve_tie_at_max_flags_tiebroken() {
        let round = round_with_votes(vec![("dev1", vec!["a"]), ("dev2", vec!["b"])]);
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let resolved = resolve(&round, Utc::now(), &mut rng).expect("must resolve");
        assert!(resolved.tiebroken);
        assert!(matches!(
            resolved.winner.place_id.as_deref(),
            Some("a") | Some("b")
        ));
    }

    #[test]
    fn resolve_empty_votes_picks_randomly_with_tiebreak() {
        let round = round_with_votes(vec![]);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let resolved = resolve(&round, Utc::now(), &mut rng).expect("must resolve");
        assert!(resolved.tiebroken);
        let pid = resolved.winner.place_id.as_deref().unwrap();
        assert!(["a", "b", "c"].contains(&pid));
    }

    #[test]
    fn resolve_approval_all_equivalent_to_approval_none() {
        // Approving everything leaves every candidate level, so it's a tie.
        let round = round_with_votes(vec![
            ("dev1", vec!["a", "b", "c"]),
            ("dev2", vec!["a", "b", "c"]),
            ("dev3", vec!["a", "b", "c"]),
        ]);
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        let resolved = resolve(&round, Utc::now(), &mut rng).expect("must resolve");
        assert!(resolved.tiebroken);
        let counts: Vec<u32> = resolved.tally.iter().map(|c| c.count).collect();
        assert_eq!(counts, vec![3, 3, 3]);
    }

    #[test]
    fn resolve_no_candidates_returns_none() {
        let mut round = round_with_votes(vec![]);
        round.candidates.clear();
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        assert!(resolve(&round, Utc::now(), &mut rng).is_none());
    }

    #[test]
    fn round_is_open_until_expiry() {
        let now = Utc::now();
        let round = Round {
            index: 0,
            candidates: vec![pub_at("a", None)],
            opened_at: now,
            expires_at: now + chrono::Duration::seconds(60),
            votes: None,
            resolved: None,
        };
        assert!(round.is_open(now));
        assert!(round.is_open(now + chrono::Duration::seconds(30)));
        assert!(!round.is_open(now + chrono::Duration::seconds(60)));
        assert!(round.is_expired(now + chrono::Duration::seconds(60)));
    }

    #[test]
    fn round_with_resolution_is_neither_open_nor_expired() {
        // Both gate transitions that only make sense while a round is open.
        let now = Utc::now();
        let resolved = ResolvedRound {
            winner: pub_at("a", None),
            tally: vec![],
            tiebroken: false,
            resolved_at: now,
        };
        let round = Round {
            index: 0,
            candidates: vec![pub_at("a", None)],
            opened_at: now,
            expires_at: now - chrono::Duration::seconds(10),
            votes: None,
            resolved: Some(resolved),
        };
        assert!(!round.is_open(now));
        assert!(!round.is_expired(now));
    }

    #[test]
    fn effective_candidates_clamp_to_range() {
        assert_eq!(
            effective_candidates_per_round(None),
            DEFAULT_CANDIDATES_PER_ROUND
        );
        assert_eq!(
            effective_candidates_per_round(Some(0)),
            MIN_CANDIDATES_PER_ROUND
        );
        assert_eq!(
            effective_candidates_per_round(Some(99)),
            MAX_CANDIDATES_PER_ROUND
        );
        assert_eq!(effective_candidates_per_round(Some(3)), 3);
    }

    #[test]
    fn effective_round_seconds_caps_at_max_preserves_zero() {
        assert_eq!(effective_round_seconds(None), DEFAULT_ROUND_SECONDS);
        // Zero means manual close.
        assert_eq!(effective_round_seconds(Some(0)), 0);
        assert_eq!(effective_round_seconds(Some(99999)), MAX_ROUND_SECONDS);
    }

    #[test]
    fn rfc3339_millis_round_trip_is_stable() {
        // Whole seconds must keep `.000`, or a lex compare against a later
        // sub-second time orders them backwards.
        let now = Utc::now();
        let s_now = now_rfc3339_millis(now);
        assert!(s_now.contains('.'), "expected millis suffix, got {s_now}");

        // Round-tripping must keep both the instant and the `.NNNZ` shape.
        let r = Round {
            index: 0,
            candidates: vec![pub_at("a", None)],
            opened_at: now,
            expires_at: now + chrono::Duration::seconds(60),
            votes: None,
            resolved: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let exp_str = r.expires_at.to_rfc3339_opts(SecondsFormat::Millis, true);
        assert!(
            json.contains(&format!("\"{exp_str}\"")),
            "expires_at not in millis form: {json}"
        );

        // Whole-second expiry against a sub-second now.
        let exp_whole = DateTime::parse_from_rfc3339("2026-05-16T12:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let now_after = DateTime::parse_from_rfc3339("2026-05-16T12:00:00.500Z")
            .unwrap()
            .with_timezone(&Utc);
        let s_exp = exp_whole.to_rfc3339_opts(SecondsFormat::Millis, true);
        let s_now = now_after.to_rfc3339_opts(SecondsFormat::Millis, true);
        // Lex order must agree with real order.
        assert!(s_now > s_exp, "{s_now} should lex > {s_exp}");
    }

    #[test]
    fn decision_mode_defaults_to_dictator() {
        // Rooms stored without a `decision_mode` must default to `Dictator`.
        let mode = DecisionMode::default();
        assert!(matches!(mode, DecisionMode::Dictator));
    }

    #[test]
    fn decision_mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&DecisionMode::Dictator).unwrap(),
            "\"dictator\""
        );
        assert_eq!(
            serde_json::to_string(&DecisionMode::Vote).unwrap(),
            "\"vote\""
        );
    }
}
