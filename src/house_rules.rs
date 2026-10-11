//! Rule strings dealt per pub when a room sets `house_rules_deck`.
//!
//! The frontend has its own copy for solo crawls; this one serves rooms, where
//! the dealt rules are persisted so every member sees the same thing. Rules are
//! stored by value, not index, so editing a deck later can't change rooms that
//! already exist.
//!
//! `Order` decides how a deck is dealt: shuffled for independent gags,
//! sequence for host-written decks that read in order, and finale for decks
//! whose last rule has to land on the last stop.

use rand::Rng;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

/// Cap on host-supplied rules. Crawls top out at 5 stops, so this is already
/// generous; the cap keeps the room JSON small, since every poll returns it.
pub const MAX_CUSTOM_RULES: usize = 12;

/// Cap on one rule's length, in characters. Keeps a full deck under a KiB.
pub const MAX_CUSTOM_RULE_LEN: usize = 140;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HouseRulesDeck {
    Tame,
    Spicy,
    Twelve,
    Custom,
}

/// How a deck's rules are handed out across a crawl.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Shuffled,
    Sequence,
    Finale,
}

/// Daft and harmless, with no drinking pressure. The safe default.
const TAME_RULES: &[&str] = &[
    "The Left Hand Pub: wrong hand only. Every sip, every crisp.",
    "The No Names Pub: first names are banned. Nicknames or descriptions only.",
    "The Posh Pub: received pronunciation at all times. You are all frightfully well-bred.",
    "The Question Pub: you may only speak in questions. Statements cost you a round?",
    "The Silent Order Pub: order your drink without saying a single word.",
    "The Toast Pub: nobody drinks until someone toasts, and no toast can be reused.",
    "The Eye Contact Pub: every clink needs eye contact with every person, or it doesn't count.",
    "The Phone Graveyard Pub: phones stacked in the middle. First to reach for theirs gets the next round.",
    "The One Word Pub: one-word answers only. Anything longer and you're out of the conversation.",
    "The Accent Pub: group picks an accent, everyone commits. No dropping it at the bar.",
    "The Whisper Pub: nothing above a whisper, including your order.",
    "The Nice Pub: everyone gives one genuine compliment to a stranger before you leave.",
    "The No Pointing Pub: indicate with elbows, chins, feet. Anything but a finger.",
    "The Sitting Down Pub: nobody stands. Not enough seats is a you problem.",
    "The Understudy Pub: you order for the person on your left and they order for you.",
    "The Empty Glass Pub: nobody leaves until every glass is finished. Pace yourselves.",
];

/// Forfeits and strangers. The escalation is social, not alcoholic.
const SPICY_RULES: &[&str] = &[
    "The Stand Up Pub: nobody sits. Not on stools, not on windowsills, not on the floor.",
    "The Pinky Pub: hold your drink with thumb and little finger only. Drop it, buy the round.",
    "The Stranger's Rule Pub: ask the next table to invent this pub's rule. Whatever they say, you're doing it.",
    "The Guinness Pub: the barman is called Guinness. \"Two lagers please, Guinness.\"",
    "The No Resting Pub: your drink never touches a surface. Not the table, not the bar.",
    "The Feed Me Pub: you can't touch your own drink. Someone else has to hold it for you.",
    "The Strangers Pub: you have never met each other. Drink like it, all round.",
    "The Judgement Pub: everyone rates the pub out of ten, out loud. Lowest score buys the next round.",
    "The Two Truths Pub: two truths and a lie each. Get caught out and you're buying.",
    "The Mimic Pub: secretly pick a stranger and adopt one of their mannerisms until you leave.",
    "The Confession Pub: one confession each before you go. Nothing you'd regret sober.",
    "The Swap Shop Pub: swap one item of clothing with someone before you order.",
    "The Dance Pub: if there's music on, you're dancing. If there isn't, someone's putting some on.",
    "The Hot Seat Pub: group picks one person. They answer any question honestly or sit the round out.",
    "The Karaoke Order Pub: every drinks order is sung to the bar.",
    "The Last One In Pub: last through the door pays for the first round at the next pub.",
];

/// The Christmas canon in traditional order. Dealt as [`Order::Finale`] so
/// pub twelve lands last however many stops there are.
const TWELVE_RULES: &[&str] = &[
    "Pub One: The No Phones Pub. Phones away, face down, out of sight. This is the easy one.",
    "Pub Two: The Shoe Swap Pub. Swap shoes with someone before you order. You keep them.",
    "Pub Three: The Wrong Hand Pub. Bad hand only, all round.",
    "Pub Four: The Silent Pub. No talking. Order your drink without words.",
    "Pub Five: The Feed Me Pub. You can't touch your own drink. Someone else holds it.",
    "Pub Six: The Accent Pub. Everyone picks an accent and commits until you leave.",
    "Pub Seven: The Shot Pub. Everyone does a shot. Lemonade counts, nobody's judging.",
    "Pub Eight: The High Ground Pub. The shortest of you stands on something so we can all see them.",
    "Pub Nine: The Guinness Pub. The barman is called Guinness. Pour you a Guinness? You're drinking it.",
    "Pub Ten: The No Resting Pub. Your drink never touches a surface.",
    "Pub Eleven: The Strangers Pub. You have never met each other in your lives.",
    "Pub Twelve: No Rules. You made it. Toast the group, buy a round, go home happy.",
];

impl HouseRulesDeck {
    /// The built-in pool. `Custom` has none; its rules come from the host.
    fn builtin_rules(&self) -> &'static [&'static str] {
        match self {
            HouseRulesDeck::Tame => TAME_RULES,
            HouseRulesDeck::Spicy => SPICY_RULES,
            HouseRulesDeck::Twelve => TWELVE_RULES,
            HouseRulesDeck::Custom => &[],
        }
    }

    fn order(&self) -> Order {
        match self {
            HouseRulesDeck::Tame | HouseRulesDeck::Spicy => Order::Shuffled,
            HouseRulesDeck::Twelve => Order::Finale,
            HouseRulesDeck::Custom => Order::Sequence,
        }
    }

    /// True when the host is expected to supply the rules themselves.
    pub fn needs_custom_rules(&self) -> bool {
        matches!(self, HouseRulesDeck::Custom)
    }

    /// The pool to draw from: built-in, or the host's own rules.
    fn pool(&self, custom: &[String]) -> Vec<String> {
        if self.needs_custom_rules() {
            custom.to_vec()
        } else {
            self.builtin_rules().iter().map(|s| s.to_string()).collect()
        }
    }
}

/// Why a custom deck was rejected. The HTTP layer collapses these to a 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomRulesError {
    /// The deck is `Custom` but no usable rules came with it.
    Empty,
    /// More than [`MAX_CUSTOM_RULES`] entries.
    TooMany { count: usize },
    /// One entry exceeded [`MAX_CUSTOM_RULE_LEN`] characters.
    RuleTooLong { index: usize, chars: usize },
}

/// Normalise and check a custom deck, returning the cleaned list so callers
/// persist exactly what gets dealt. Blank entries are dropped rather than
/// rejected, because a text area produces trailing empty lines.
pub fn validate_custom_rules(rules: &[String]) -> Result<Vec<String>, CustomRulesError> {
    let cleaned: Vec<String> = rules
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .map(|r| r.to_string())
        .collect();

    if cleaned.is_empty() {
        return Err(CustomRulesError::Empty);
    }
    if cleaned.len() > MAX_CUSTOM_RULES {
        return Err(CustomRulesError::TooMany {
            count: cleaned.len(),
        });
    }
    for (index, rule) in cleaned.iter().enumerate() {
        let chars = rule.chars().count();
        if chars > MAX_CUSTOM_RULE_LEN {
            return Err(CustomRulesError::RuleTooLong { index, chars });
        }
    }
    Ok(cleaned)
}

/// Deal `count` rules from `pool`, front-to-back, wrapping if the pool is
/// short. With `finale`, the pool's last entry is held back for the last stop,
/// so a 3-stop 12 Pubs crawl gets pubs one, two and twelve.
fn deal_in_order(pool: &[String], count: usize, finale: bool) -> Vec<String> {
    if finale && count >= 2 && count < pool.len() {
        let mut out: Vec<String> = pool[..count - 1].to_vec();
        out.push(pool[pool.len() - 1].clone());
        return out;
    }
    (0..count).map(|i| pool[i % pool.len()].clone()).collect()
}

/// Roll `count` rules from `deck`, using `custom` for a
/// [`HouseRulesDeck::Custom`] deck. Always returns exactly `count` items:
/// shuffled decks draw without replacement until the deck runs out, then with.
/// Ordered decks ignore `rng`.
pub fn roll_rules<R: Rng>(
    deck: HouseRulesDeck,
    custom: &[String],
    count: usize,
    rng: &mut R,
) -> Vec<String> {
    let pool = deck.pool(custom);
    if count == 0 || pool.is_empty() {
        return Vec::new();
    }

    match deck.order() {
        Order::Sequence => deal_in_order(&pool, count, false),
        Order::Finale => deal_in_order(&pool, count, true),
        Order::Shuffled => {
            let mut shuffled = pool.clone();
            shuffled.shuffle(rng);

            let take = count.min(shuffled.len());
            let mut out: Vec<String> = shuffled.into_iter().take(take).collect();

            // More rules wanted than the deck holds: top up with replacement.
            while out.len() < count {
                out.push(pool[rng.gen_range(0..pool.len())].clone());
            }
            out
        }
    }
}

/// Roll the next rule for a crawl of `total` stops that already dealt
/// `history`. Vote-mode rooms deal one rule per resolved round instead of all
/// of them at start. Ordered decks index into the deal for `total`, so the arc
/// matches dictator mode; shuffled decks avoid `history` until the deck runs
/// out.
pub fn roll_one<R: Rng>(
    deck: HouseRulesDeck,
    custom: &[String],
    history: &[String],
    total: usize,
    rng: &mut R,
) -> String {
    let pool = deck.pool(custom);
    if pool.is_empty() {
        // Only a `Custom` deck that escaped validation can land here.
        return String::new();
    }

    let idx = history.len();

    if deck.order() != Order::Shuffled {
        // A room that outruns `total` still needs an answer, so widen.
        let deal = deal_in_order(&pool, total.max(idx + 1), deck.order() == Order::Finale);
        return deal[idx].clone();
    }

    // Decks are ~16 strings, so a HashSet wouldn't pay off.
    let unused: Vec<&String> = pool.iter().filter(|s| !history.contains(s)).collect();

    if let Some(pick) = unused.choose(rng) {
        return (*pick).clone();
    }

    // Deck exhausted, so allow repeats.
    pool[rng.gen_range(0..pool.len())].clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use std::collections::HashSet;

    fn rng() -> rand::rngs::StdRng {
        rand::rngs::StdRng::seed_from_u64(42)
    }

    fn owned(rules: &[&str]) -> Vec<String> {
        rules.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn roll_rules_distinct_when_count_under_deck_size() {
        let mut r = rng();
        let out = roll_rules(HouseRulesDeck::Tame, &[], 5, &mut r);
        assert_eq!(out.len(), 5);
        let unique: HashSet<&String> = out.iter().collect();
        assert_eq!(unique.len(), 5, "expected distinct rules: {out:?}");
    }

    #[test]
    fn roll_rules_fills_with_replacement_when_count_exceeds_deck() {
        let mut r = rng();
        let deck_len = TAME_RULES.len();
        let out = roll_rules(HouseRulesDeck::Tame, &[], deck_len + 3, &mut r);
        assert_eq!(out.len(), deck_len + 3);
        let pool: HashSet<&str> = TAME_RULES.iter().copied().collect();
        for rule in &out {
            assert!(
                pool.contains(rule.as_str()),
                "off-deck rule leaked in: {rule}"
            );
        }
    }

    #[test]
    fn roll_rules_only_returns_strings_from_specified_deck() {
        let mut r = rng();
        let out = roll_rules(HouseRulesDeck::Spicy, &[], 5, &mut r);
        let spicy: HashSet<&str> = SPICY_RULES.iter().copied().collect();
        for rule in &out {
            assert!(spicy.contains(rule.as_str()), "expected spicy rule: {rule}");
        }
    }

    #[test]
    fn roll_rules_handles_zero_count() {
        let mut r = rng();
        assert!(roll_rules(HouseRulesDeck::Tame, &[], 0, &mut r).is_empty());
    }

    #[test]
    fn deck_serializes_to_lowercase() {
        for (deck, wire) in [
            (HouseRulesDeck::Tame, "\"tame\""),
            (HouseRulesDeck::Spicy, "\"spicy\""),
            (HouseRulesDeck::Twelve, "\"twelve\""),
            (HouseRulesDeck::Custom, "\"custom\""),
        ] {
            assert_eq!(serde_json::to_string(&deck).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<HouseRulesDeck>(wire).unwrap(),
                deck,
                "round-trip failed for {wire}"
            );
        }
    }

    #[test]
    fn roll_one_avoids_history_until_deck_exhausted() {
        let mut r = rng();
        let mut history: Vec<String> = Vec::new();
        let deck_len = TAME_RULES.len();

        for _ in 0..deck_len {
            let next = roll_one(HouseRulesDeck::Tame, &[], &history, deck_len, &mut r);
            assert!(
                !history.contains(&next),
                "roll_one returned a used rule before deck exhausted: {next}"
            );
            history.push(next);
        }
        let unique: HashSet<&String> = history.iter().collect();
        assert_eq!(unique.len(), deck_len);
    }

    #[test]
    fn roll_one_falls_back_to_replacement_when_exhausted() {
        let mut r = rng();
        let history = owned(TAME_RULES);
        let next = roll_one(HouseRulesDeck::Tame, &[], &history, history.len(), &mut r);
        assert!(
            TAME_RULES.contains(&next.as_str()),
            "off-deck rule leaked in: {next}"
        );
    }

    #[test]
    fn twelve_deck_keeps_canonical_order_and_ends_on_the_finale() {
        let mut r = rng();
        let out = roll_rules(HouseRulesDeck::Twelve, &[], 3, &mut r);
        assert_eq!(
            out,
            vec![
                TWELVE_RULES[0].to_string(),
                TWELVE_RULES[1].to_string(),
                TWELVE_RULES[11].to_string(),
            ],
            "3-stop crawl should be pubs one, two, twelve"
        );
    }

    #[test]
    fn twelve_deck_full_length_is_the_deck_verbatim() {
        let mut r = rng();
        let out = roll_rules(HouseRulesDeck::Twelve, &[], TWELVE_RULES.len(), &mut r);
        assert_eq!(out, owned(TWELVE_RULES));
    }

    #[test]
    fn twelve_deck_single_stop_gets_pub_one() {
        // A 1-stop deal has no room to reserve a finale slot.
        let mut r = rng();
        let out = roll_rules(HouseRulesDeck::Twelve, &[], 1, &mut r);
        assert_eq!(out, vec![TWELVE_RULES[0].to_string()]);
    }

    #[test]
    fn twelve_deck_is_deterministic_across_seeds() {
        let a = roll_rules(
            HouseRulesDeck::Twelve,
            &[],
            4,
            &mut rand::rngs::StdRng::seed_from_u64(1),
        );
        let b = roll_rules(
            HouseRulesDeck::Twelve,
            &[],
            4,
            &mut rand::rngs::StdRng::seed_from_u64(9_999),
        );
        assert_eq!(a, b, "ordered decks must not depend on the rng");
    }

    #[test]
    fn vote_mode_ordered_roll_matches_the_all_at_once_deal() {
        let mut r = rng();
        let total = 4;
        let expected = roll_rules(HouseRulesDeck::Twelve, &[], total, &mut r);

        let mut history: Vec<String> = Vec::new();
        for _ in 0..total {
            let next = roll_one(HouseRulesDeck::Twelve, &[], &history, total, &mut r);
            history.push(next);
        }
        assert_eq!(history, expected);
    }

    #[test]
    fn ordered_roll_one_past_total_still_returns_a_rule() {
        let mut r = rng();
        let history = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        // Fourth roll of a 2-stop crawl must widen, not panic.
        let next = roll_one(HouseRulesDeck::Twelve, &[], &history, 2, &mut r);
        assert!(!next.is_empty());
    }

    #[test]
    fn custom_deck_deals_host_rules_in_order() {
        let mut r = rng();
        let custom = owned(&["one", "two", "three"]);
        let out = roll_rules(HouseRulesDeck::Custom, &custom, 3, &mut r);
        assert_eq!(out, custom);
    }

    #[test]
    fn custom_deck_wraps_when_shorter_than_the_crawl() {
        let mut r = rng();
        let custom = owned(&["one", "two"]);
        let out = roll_rules(HouseRulesDeck::Custom, &custom, 5, &mut r);
        assert_eq!(out, owned(&["one", "two", "one", "two", "one"]));
    }

    #[test]
    fn custom_deck_takes_a_prefix_when_longer_than_the_crawl() {
        let mut r = rng();
        let custom = owned(&["one", "two", "three", "four"]);
        let out = roll_rules(HouseRulesDeck::Custom, &custom, 2, &mut r);
        assert_eq!(
            out,
            owned(&["one", "two"]),
            "custom decks are a sequence, not an arc, so no finale reservation"
        );
    }

    #[test]
    fn custom_deck_with_no_rules_yields_nothing() {
        let mut r = rng();
        assert!(roll_rules(HouseRulesDeck::Custom, &[], 3, &mut r).is_empty());
        assert_eq!(roll_one(HouseRulesDeck::Custom, &[], &[], 3, &mut r), "");
    }

    #[test]
    fn custom_deck_ignores_builtin_pools() {
        let mut r = rng();
        let custom = owned(&["mine"]);
        let out = roll_rules(HouseRulesDeck::Custom, &custom, 3, &mut r);
        for rule in &out {
            assert_eq!(rule, "mine");
        }
    }

    #[test]
    fn builtin_decks_ignore_supplied_custom_rules() {
        let mut r = rng();
        let custom = owned(&["should never appear"]);
        let out = roll_rules(HouseRulesDeck::Tame, &custom, 4, &mut r);
        for rule in &out {
            assert!(TAME_RULES.contains(&rule.as_str()), "leaked: {rule}");
        }
    }

    #[test]
    fn validate_custom_rules_trims_and_drops_blanks() {
        let input = owned(&["  first  ", "", "   ", "second"]);
        assert_eq!(
            validate_custom_rules(&input).unwrap(),
            owned(&["first", "second"])
        );
    }

    #[test]
    fn validate_custom_rules_rejects_empty() {
        assert_eq!(validate_custom_rules(&[]), Err(CustomRulesError::Empty));
        assert_eq!(
            validate_custom_rules(&owned(&["  ", ""])),
            Err(CustomRulesError::Empty)
        );
    }

    #[test]
    fn validate_custom_rules_rejects_too_many() {
        let input: Vec<String> = (0..MAX_CUSTOM_RULES + 1).map(|i| format!("r{i}")).collect();
        assert_eq!(
            validate_custom_rules(&input),
            Err(CustomRulesError::TooMany {
                count: MAX_CUSTOM_RULES + 1
            })
        );
    }

    #[test]
    fn validate_custom_rules_accepts_exactly_the_cap() {
        let input: Vec<String> = (0..MAX_CUSTOM_RULES).map(|i| format!("r{i}")).collect();
        assert_eq!(
            validate_custom_rules(&input).unwrap().len(),
            MAX_CUSTOM_RULES
        );
    }

    #[test]
    fn validate_custom_rules_rejects_overlong_rule() {
        let input = vec!["ok".to_string(), "x".repeat(MAX_CUSTOM_RULE_LEN + 1)];
        assert_eq!(
            validate_custom_rules(&input),
            Err(CustomRulesError::RuleTooLong {
                index: 1,
                chars: MAX_CUSTOM_RULE_LEN + 1
            })
        );
    }

    #[test]
    fn validate_custom_rules_counts_chars_not_bytes() {
        // Counted in characters, so multi-byte content at the cap passes.
        let input = vec!["é".repeat(MAX_CUSTOM_RULE_LEN)];
        assert!(validate_custom_rules(&input).is_ok());
    }

    #[test]
    fn builtin_rules_are_within_the_custom_length_cap() {
        for deck in [
            HouseRulesDeck::Tame,
            HouseRulesDeck::Spicy,
            HouseRulesDeck::Twelve,
        ] {
            for rule in deck.builtin_rules() {
                assert!(
                    rule.chars().count() <= MAX_CUSTOM_RULE_LEN,
                    "rule too long for the UI slot: {rule}"
                );
            }
        }
    }

    #[test]
    fn builtin_decks_have_no_duplicates() {
        for deck in [
            HouseRulesDeck::Tame,
            HouseRulesDeck::Spicy,
            HouseRulesDeck::Twelve,
        ] {
            let rules = deck.builtin_rules();
            let unique: HashSet<&&str> = rules.iter().collect();
            assert_eq!(unique.len(), rules.len(), "duplicate rule in {deck:?}");
        }
    }

    #[test]
    fn twelve_deck_has_twelve_rules() {
        assert_eq!(TWELVE_RULES.len(), 12);
    }
}
