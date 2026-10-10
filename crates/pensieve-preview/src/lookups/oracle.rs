//! Independent Rust reference implementations of the compact lookup rules.
//!
//! Validation and tests compare ClickHouse results against these functions
//! computed from the raw source events, so a bug in the SQL cannot hide behind
//! the same bug in the expected values.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use regex::Regex;

use crate::query::ZapTotals;

/// Zap amounts above this many msats are treated as fake and counted as 0
/// (migration 009).
pub const MAX_ZAP_AMOUNT_MSATS: u64 = 1_000_000_000;

/// A source event as stored in `events_local`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEvent {
    /// Event id (hex).
    pub id: String,
    /// Author pubkey (hex).
    pub pubkey: String,
    /// Unix `created_at`.
    pub created_at: u32,
    /// Event kind.
    pub kind: u16,
    /// Event content.
    pub content: String,
    /// Event tags.
    pub tags: Vec<Vec<String>>,
}

/// Pick the replaceable-event winner: highest `created_at`, then lowest id
/// (NIP-01). Duplicates of the same event are harmless.
pub fn replaceable_winner<'a>(
    events: impl IntoIterator<Item = &'a SourceEvent>,
) -> Option<&'a SourceEvent> {
    events.into_iter().min_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    })
}

/// Number of distinct event ids sharing the winning `created_at`.
pub fn tie_width<'a>(events: impl IntoIterator<Item = &'a SourceEvent> + Clone) -> usize {
    let Some(max) = events.clone().into_iter().map(|e| e.created_at).max() else {
        return 0;
    };
    let mut ids: Vec<&str> = events
        .into_iter()
        .filter(|e| e.created_at == max)
        .map(|e| e.id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.len()
}

/// First `d` tag value, matching the `events_local.d_tag` expression
/// (`''` when absent or valueless).
pub fn d_tag(tags: &[Vec<String>]) -> &str {
    tags.iter()
        .find(|t| t.first().map(String::as_str) == Some("d"))
        .and_then(|t| t.get(1))
        .map_or("", String::as_str)
}

/// Distinct non-empty `e` tag values, in first-seen order.
pub fn zap_targets(tags: &[Vec<String>]) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    for tag in tags {
        if tag.len() >= 2 && tag[0] == "e" && !tag[1].is_empty() && !targets.contains(&tag[1]) {
            targets.push(tag[1].clone());
        }
    }
    targets
}

static MULTIPLIERS: LazyLock<[(Regex, Multiplier); 4]> = LazyLock::new(|| {
    [
        (re(r"^lnbc([0-9]+)m1"), Multiplier::Mul(100_000_000)),
        (re(r"^lnbc([0-9]+)u1"), Multiplier::Mul(100_000)),
        (re(r"^lnbc([0-9]+)n1"), Multiplier::Mul(100)),
        (re(r"^lnbc([0-9]+)p1"), Multiplier::Div(10)),
    ]
});

static PARSEABLE: LazyLock<Regex> = LazyLock::new(|| re(r"^lnbc[0-9]+[munp]1"));

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex")
}

#[derive(Clone, Copy)]
enum Multiplier {
    Mul(u64),
    Div(u64),
}

/// Parse a zap receipt amount with the migration 009 rules.
///
/// Returns `(amount_msats, parsed)`:
/// - the amount comes from the *first* `bolt11` tag, using the first matching
///   multiplier in m, u, n, p order; integer overflow follows ClickHouse
///   (`toUInt64OrZero` gives 0 and multiplication wraps);
/// - amounts above [`MAX_ZAP_AMOUNT_MSATS`] become 0;
/// - `parsed` is true when *any* `bolt11` tag matches `^lnbc[0-9]+[munp]1`,
///   which is the legacy inclusion rule for counting a receipt.
pub fn zap_amount(tags: &[Vec<String>]) -> (u64, bool) {
    let first_bolt11 = tags
        .iter()
        .find(|t| t.first().map(String::as_str) == Some("bolt11"))
        .and_then(|t| t.get(1))
        .map_or("", String::as_str);

    let mut raw = 0u64;
    for (regex, multiplier) in MULTIPLIERS.iter() {
        let digits = regex
            .captures(first_bolt11)
            .and_then(|c| c.get(1))
            .map_or("", |m| m.as_str());
        if digits.is_empty() {
            continue;
        }
        let value = digits.parse::<u64>().unwrap_or(0);
        raw = match multiplier {
            Multiplier::Mul(factor) => value.wrapping_mul(*factor),
            Multiplier::Div(divisor) => value / divisor,
        };
        break;
    }
    let amount = if raw > MAX_ZAP_AMOUNT_MSATS { 0 } else { raw };

    let parsed = tags
        .iter()
        .any(|t| t.len() >= 2 && t[0] == "bolt11" && PARSEABLE.is_match(&t[1]));

    (amount, parsed)
}

/// Expected zap totals per target from a set of kind 9735 receipts.
///
/// Receipts are deduplicated by id, so replays never inflate totals.
pub fn zap_totals_by_target<'a>(
    receipts: impl IntoIterator<Item = &'a SourceEvent>,
) -> HashMap<String, ZapTotals> {
    let mut unique: BTreeMap<&str, &SourceEvent> = BTreeMap::new();
    for receipt in receipts {
        if receipt.kind == 9735 {
            unique.entry(receipt.id.as_str()).or_insert(receipt);
        }
    }

    let mut totals: HashMap<String, ZapTotals> = HashMap::new();
    for receipt in unique.values() {
        let (amount, parsed) = zap_amount(&receipt.tags);
        if !parsed {
            continue;
        }
        for target in zap_targets(&receipt.tags) {
            let entry = totals.entry(target).or_default();
            entry.zap_count += 1;
            entry.total_msats = entry.total_msats.wrapping_add(amount);
        }
    }
    totals
}

/// Expected latest profile per pubkey from kind 0 source events.
pub fn latest_profiles<'a>(
    events: impl IntoIterator<Item = &'a SourceEvent>,
) -> HashMap<String, &'a SourceEvent> {
    let mut grouped: HashMap<&str, Vec<&SourceEvent>> = HashMap::new();
    for event in events {
        if event.kind == 0 {
            grouped
                .entry(event.pubkey.as_str())
                .or_default()
                .push(event);
        }
    }
    grouped
        .into_iter()
        .filter_map(|(pubkey, events)| {
            replaceable_winner(events).map(|winner| (pubkey.to_string(), winner))
        })
        .collect()
}

/// Expected latest event per (pubkey, kind, d_tag) for kinds covered by
/// `addressable_latest`.
pub fn latest_addressable<'a>(
    events: impl IntoIterator<Item = &'a SourceEvent>,
) -> HashMap<(String, u16, String), &'a SourceEvent> {
    let mut grouped: HashMap<(String, u16, String), Vec<&SourceEvent>> = HashMap::new();
    for event in events {
        if super::compact::addressable_kind_covered(event.kind) {
            let key = (
                event.pubkey.clone(),
                event.kind,
                d_tag(&event.tags).to_string(),
            );
            grouped.entry(key).or_default().push(event);
        }
    }
    grouped
        .into_iter()
        .filter_map(|(key, events)| replaceable_winner(events).map(|winner| (key, winner)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(raw: &[&[&str]]) -> Vec<Vec<String>> {
        raw.iter()
            .map(|t| t.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    fn event(id: &str, created_at: u32) -> SourceEvent {
        SourceEvent {
            id: id.to_string(),
            pubkey: "p".to_string(),
            created_at,
            kind: 0,
            content: id.to_string(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn winner_prefers_newest_then_lowest_id() {
        let events = [
            event("b", 10),
            event("a", 9),
            event("c", 10),
            event("b", 10),
        ];
        assert_eq!(replaceable_winner(&events).unwrap().id, "b");
        assert_eq!(tie_width(&events), 2);

        let reversed: Vec<_> = events.iter().rev().cloned().collect();
        assert_eq!(replaceable_winner(&reversed).unwrap().id, "b");
    }

    #[test]
    fn winner_of_empty_is_none() {
        let none: [SourceEvent; 0] = [];
        assert!(replaceable_winner(&none).is_none());
        assert_eq!(tie_width(&none), 0);
    }

    #[test]
    fn d_tag_matches_clickhouse_first_d_semantics() {
        assert_eq!(
            d_tag(&tags(&[&["t", "x"], &["d", "slug"], &["d", "other"]])),
            "slug"
        );
        assert_eq!(d_tag(&tags(&[&["d"], &["d", "slug"]])), "");
        assert_eq!(d_tag(&tags(&[])), "");
    }

    #[test]
    fn zap_targets_are_distinct_and_non_empty() {
        let t = tags(&[
            &["e", "a"],
            &["e"],
            &["e", ""],
            &["e", "a"],
            &["e", "b", "relay"],
            &[],
        ]);
        assert_eq!(zap_targets(&t), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn zap_amount_multipliers() {
        assert_eq!(
            zap_amount(&tags(&[&["bolt11", "lnbc2m1xyz"]])),
            (200_000_000, true)
        );
        assert_eq!(
            zap_amount(&tags(&[&["bolt11", "lnbc21u1xyz"]])),
            (2_100_000, true)
        );
        assert_eq!(
            zap_amount(&tags(&[&["bolt11", "lnbc210n1xyz"]])),
            (21_000, true)
        );
        assert_eq!(
            zap_amount(&tags(&[&["bolt11", "lnbc2105p1xyz"]])),
            (210, true)
        );
    }

    #[test]
    fn zap_amount_limits_and_rejections() {
        // Above 1B msats is zeroed but still counted.
        assert_eq!(zap_amount(&tags(&[&["bolt11", "lnbc11m1x"]])), (0, true));
        // Exactly 1B msats is kept.
        assert_eq!(
            zap_amount(&tags(&[&["bolt11", "lnbc10m1x"]])),
            (1_000_000_000, true)
        );
        // No multiplier: not parsed.
        assert_eq!(zap_amount(&tags(&[&["bolt11", "lnbc1500x"]])), (0, false));
        // Testnet invoices are not parsed.
        assert_eq!(zap_amount(&tags(&[&["bolt11", "lntb10u1x"]])), (0, false));
        // u64 overflow in the digits gives 0 (toUInt64OrZero).
        assert_eq!(
            zap_amount(&tags(&[&["bolt11", "lnbc99999999999999999999999m1x"]])),
            (0, true)
        );
        // Missing / valueless bolt11.
        assert_eq!(zap_amount(&tags(&[&["e", "x"]])), (0, false));
        assert_eq!(zap_amount(&tags(&[&["bolt11"]])), (0, false));
    }

    #[test]
    fn zap_amount_uses_first_bolt11_but_any_for_inclusion() {
        // First bolt11 is garbage, second is valid: counted with amount 0.
        let t = tags(&[&["bolt11", "garbage"], &["bolt11", "lnbc5u1x"]]);
        assert_eq!(zap_amount(&t), (0, true));
        // First valueless bolt11 also shadows later ones for the amount.
        let t = tags(&[&["bolt11"], &["bolt11", "lnbc5u1x"]]);
        assert_eq!(zap_amount(&t), (0, true));
    }

    #[test]
    fn zap_totals_dedupe_replays() {
        let receipt = SourceEvent {
            id: "r1".to_string(),
            pubkey: "z".to_string(),
            created_at: 1,
            kind: 9735,
            content: String::new(),
            tags: tags(&[&["bolt11", "lnbc10n1x"], &["e", "t"], &["e", "t"]]),
        };
        let other = SourceEvent {
            id: "r2".to_string(),
            tags: tags(&[&["bolt11", "lnbc20n1x"], &["e", "t"], &["e", "u"]]),
            ..receipt.clone()
        };
        let unparsed = SourceEvent {
            id: "r3".to_string(),
            tags: tags(&[&["bolt11", "nope"], &["e", "t"]]),
            ..receipt.clone()
        };
        let totals = zap_totals_by_target([&receipt, &receipt, &other, &unparsed, &other]);
        assert_eq!(
            totals["t"],
            ZapTotals {
                total_msats: 3_000,
                zap_count: 2
            }
        );
        assert_eq!(
            totals["u"],
            ZapTotals {
                total_msats: 2_000,
                zap_count: 1
            }
        );
    }
}
