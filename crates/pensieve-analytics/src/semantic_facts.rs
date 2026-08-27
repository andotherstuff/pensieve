//! Exact, bounded-state semantic classification for Slice 7 analytics.
//!
//! These helpers deliberately separate canonical event classification from
//! aggregation and publication.  A production scan can therefore account for
//! every input event ID while retaining only fixed-size counters and additive
//! period keys.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Maximum accepted zap amount, matching the current production safety gate.
pub const MAX_ZAP_AMOUNT_MSATS: u64 = 1_000_000_000;

/// Inclusive whole-satoshi upper bounds used by the public histogram.
pub const ZAP_HISTOGRAM_UPPER_SATS: [u64; 16] = [
    10, 21, 50, 100, 250, 500, 750, 1_000, 2_500, 5_000, 7_500, 10_000, 25_000, 50_000, 75_000,
    100_000,
];

/// Classification of one event for the engagement product.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngagementFact {
    /// Kind-1 event with at least one tag whose first element is `e`.
    Reply,
    /// Kind-1 event without an `e` tag.
    OriginalNote,
    /// Kind-7 reaction event.
    Reaction,
    /// Event outside the engagement product.
    Other,
}

/// Classify engagement exactly as the existing endpoint does.
pub fn classify_engagement(kind: u16, tags: &[Vec<String>]) -> EngagementFact {
    match kind {
        1 if tags
            .iter()
            .any(|tag| tag.first().is_some_and(|name| name == "e")) =>
        {
            EngagementFact::Reply
        }
        1 => EngagementFact::OriginalNote,
        7 => EngagementFact::Reaction,
        _ => EngagementFact::Other,
    }
}

/// Additive facts for one long-form event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LongformFact {
    /// Exact UTF-8 byte length, matching ClickHouse `length(String)`.
    pub content_bytes: u64,
}

/// Return a long-form fact for kind 30023.
pub fn classify_longform(kind: u16, content: &str) -> Option<LongformFact> {
    (kind == 30_023).then_some(LongformFact {
        content_bytes: content.len() as u64,
    })
}

/// Why a kind-9735 event did not produce a positive canonical zap fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZapRejection {
    /// No positional `bolt11` tag exists.
    MissingBolt11,
    /// The first positional `bolt11` tag has no value.
    MissingBolt11Value,
    /// The first invoice does not use the accepted lowercase mainnet shape.
    MalformedBolt11,
    /// Decimal parsing or multiplier conversion overflowed `u64`.
    AmountOverflow,
    /// Pico conversion truncated the amount to zero millisatoshis.
    ZeroAmount,
    /// The parsed amount exceeds the production anti-abuse ceiling.
    AmountAboveLimit,
}

impl ZapRejection {
    const COUNT: usize = 6;

    fn ordinal(self) -> usize {
        match self {
            Self::MissingBolt11 => 0,
            Self::MissingBolt11Value => 1,
            Self::MalformedBolt11 => 2,
            Self::AmountOverflow => 3,
            Self::ZeroAmount => 4,
            Self::AmountAboveLimit => 5,
        }
    }
}

/// One accepted, additive zap fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZapFact {
    /// Exact parsed amount in millisatoshis.
    pub amount_msats: u64,
    /// First positional lowercase `p` tag, if present.
    pub recipient: Option<String>,
    /// First positional uppercase `P` tag, if present.
    pub sender: Option<String>,
    /// Validated 32-byte recipient key, when the positional value is hex.
    pub recipient_pubkey: Option<[u8; 32]>,
    /// Validated 32-byte sender key, when the positional value is hex.
    pub sender_pubkey: Option<[u8; 32]>,
    /// Fixed public histogram bucket ordinal in the range 0..=16.
    pub histogram_bucket: u8,
}

/// Classify one event into the canonical parsed-zap domain.
///
/// Like ClickHouse migration 009, the first positional `bolt11`, `p`, and `P`
/// tags own their respective values.  Later tags do not repair an invalid first
/// value.  Unlike the old derived table, valid pubkeys are recorded separately
/// so missing/malformed identities can be quantified without inventing a
/// distinct participant.
pub fn classify_zap(kind: u16, tags: &[Vec<String>]) -> Option<Result<ZapFact, ZapRejection>> {
    if kind != 9_735 {
        return None;
    }
    let bolt11 = first_tag_value(tags, "bolt11");
    let invoice = match bolt11 {
        None => return Some(Err(ZapRejection::MissingBolt11)),
        Some(None) => return Some(Err(ZapRejection::MissingBolt11Value)),
        Some(Some(value)) => value,
    };
    let amount_msats = match parse_bolt11_msats(invoice) {
        Ok(amount) => amount,
        Err(rejection) => return Some(Err(rejection)),
    };
    let recipient = first_tag_value(tags, "p").flatten().map(str::to_owned);
    let sender = first_tag_value(tags, "P").flatten().map(str::to_owned);
    Some(Ok(ZapFact {
        amount_msats,
        recipient_pubkey: recipient.as_deref().and_then(parse_pubkey),
        sender_pubkey: sender.as_deref().and_then(parse_pubkey),
        recipient,
        sender,
        histogram_bucket: zap_histogram_bucket(amount_msats),
    }))
}

fn first_tag_value<'a>(tags: &'a [Vec<String>], name: &str) -> Option<Option<&'a str>> {
    tags.iter()
        .find(|tag| tag.first().is_some_and(|candidate| candidate == name))
        .map(|tag| tag.get(1).map(String::as_str))
}

fn parse_bolt11_msats(invoice: &str) -> Result<u64, ZapRejection> {
    let suffix = invoice
        .strip_prefix("lnbc")
        .ok_or(ZapRejection::MalformedBolt11)?;
    let digit_count = suffix.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return Err(ZapRejection::MalformedBolt11);
    }
    let amount = suffix[..digit_count]
        .parse::<u64>()
        .map_err(|_| ZapRejection::AmountOverflow)?;
    let remainder = &suffix[digit_count..];
    let (multiplier, payload) = remainder
        .split_at_checked(1)
        .ok_or(ZapRejection::MalformedBolt11)?;
    if !payload.starts_with('1') {
        return Err(ZapRejection::MalformedBolt11);
    }
    let amount_msats = match multiplier.as_bytes()[0] {
        b'm' => amount.checked_mul(100_000_000),
        b'u' => amount.checked_mul(100_000),
        b'n' => amount.checked_mul(100),
        b'p' => Some(amount / 10),
        _ => return Err(ZapRejection::MalformedBolt11),
    }
    .ok_or(ZapRejection::AmountOverflow)?;
    if amount_msats == 0 {
        return Err(ZapRejection::ZeroAmount);
    }
    if amount_msats > MAX_ZAP_AMOUNT_MSATS {
        return Err(ZapRejection::AmountAboveLimit);
    }
    Ok(amount_msats)
}

/// Return the fixed public histogram bucket for a positive millisatoshi value.
pub fn zap_histogram_bucket(amount_msats: u64) -> u8 {
    let sats = amount_msats / 1_000;
    ZAP_HISTOGRAM_UPPER_SATS.partition_point(|upper| sats > *upper) as u8
}

/// Exact additive engagement counters for one UTC day.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EngagementDay {
    /// UTC day start as Unix seconds.
    pub day_epoch: u64,
    /// Kind-1 events without an `e` tag.
    pub original_notes: u64,
    /// Kind-1 events with an `e` tag.
    pub replies: u64,
    /// Kind-7 events.
    pub reactions: u64,
}

/// Exact additive long-form counters for one UTC day.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LongformDay {
    /// UTC day start as Unix seconds.
    pub day_epoch: u64,
    /// Kind-30023 event count.
    pub articles: u64,
    /// Sum of exact UTF-8 content bytes.
    pub content_bytes: u64,
}

/// Exact additive zap counters for one UTC day.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ZapDay {
    /// UTC day start as Unix seconds.
    pub day_epoch: u64,
    /// Accepted positive zap facts.
    pub accepted: u64,
    /// Sum of accepted amounts in millisatoshis.
    pub amount_msats: u64,
    /// Accepted facts with a validated sender key.
    pub validated_senders: u64,
    /// Accepted facts with a validated recipient key.
    pub validated_recipients: u64,
    /// Exact counts in the 17 fixed histogram buckets.
    pub histogram: [u64; 17],
    /// Rejected kind-9735 events, indexed by [`ZapRejection`].
    pub rejected: [u64; ZapRejection::COUNT],
}

/// Bounded Slice 7 additive state keyed only by UTC day.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticRollups {
    /// Engagement counters by UTC day.
    pub engagement: BTreeMap<u64, EngagementDay>,
    /// Long-form counters by UTC day.
    pub longform: BTreeMap<u64, LongformDay>,
    /// Zap counters and rejection accounting by UTC day.
    pub zaps: BTreeMap<u64, ZapDay>,
}

impl SemanticRollups {
    /// Account for one already-deduplicated canonical event.
    pub fn observe(
        &mut self,
        created_at: u64,
        kind: u16,
        tags: &[Vec<String>],
        content: &str,
    ) -> Result<(), &'static str> {
        let day_epoch = created_at - created_at % 86_400;
        match classify_engagement(kind, tags) {
            EngagementFact::Reply => checked_increment(
                &mut self.engagement_day(day_epoch).replies,
                "engagement replies overflowed",
            )?,
            EngagementFact::OriginalNote => checked_increment(
                &mut self.engagement_day(day_epoch).original_notes,
                "engagement original notes overflowed",
            )?,
            EngagementFact::Reaction => checked_increment(
                &mut self.engagement_day(day_epoch).reactions,
                "engagement reactions overflowed",
            )?,
            EngagementFact::Other => {}
        }
        if let Some(fact) = classify_longform(kind, content) {
            let day = self.longform_day(day_epoch);
            checked_increment(&mut day.articles, "long-form article count overflowed")?;
            day.content_bytes = day
                .content_bytes
                .checked_add(fact.content_bytes)
                .ok_or("long-form content bytes overflowed")?;
        }
        if let Some(result) = classify_zap(kind, tags) {
            let day = self.zap_day(day_epoch);
            match result {
                Ok(fact) => {
                    checked_increment(&mut day.accepted, "accepted zap count overflowed")?;
                    day.amount_msats = day
                        .amount_msats
                        .checked_add(fact.amount_msats)
                        .ok_or("zap amount sum overflowed")?;
                    if fact.sender_pubkey.is_some() {
                        checked_increment(
                            &mut day.validated_senders,
                            "validated sender count overflowed",
                        )?;
                    }
                    if fact.recipient_pubkey.is_some() {
                        checked_increment(
                            &mut day.validated_recipients,
                            "validated recipient count overflowed",
                        )?;
                    }
                    checked_increment(
                        &mut day.histogram[usize::from(fact.histogram_bucket)],
                        "zap histogram count overflowed",
                    )?;
                }
                Err(rejection) => checked_increment(
                    &mut day.rejected[rejection.ordinal()],
                    "zap rejection count overflowed",
                )?,
            }
        }
        Ok(())
    }

    fn engagement_day(&mut self, day_epoch: u64) -> &mut EngagementDay {
        self.engagement.entry(day_epoch).or_insert(EngagementDay {
            day_epoch,
            ..EngagementDay::default()
        })
    }

    fn longform_day(&mut self, day_epoch: u64) -> &mut LongformDay {
        self.longform.entry(day_epoch).or_insert(LongformDay {
            day_epoch,
            ..LongformDay::default()
        })
    }

    fn zap_day(&mut self, day_epoch: u64) -> &mut ZapDay {
        self.zaps.entry(day_epoch).or_insert(ZapDay {
            day_epoch,
            ..ZapDay::default()
        })
    }
}

fn checked_increment(value: &mut u64, message: &'static str) -> Result<(), &'static str> {
    *value = value.checked_add(1).ok_or(message)?;
    Ok(())
}

fn parse_pubkey(value: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(value).ok()?;
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(values: &[&[&str]]) -> Vec<Vec<String>> {
        values
            .iter()
            .map(|tag| tag.iter().map(|value| (*value).to_owned()).collect())
            .collect()
    }

    #[test]
    fn engagement_matches_positional_tag_semantics() {
        assert_eq!(
            classify_engagement(1, &tags(&[&["e"]])),
            EngagementFact::Reply
        );
        assert_eq!(
            classify_engagement(1, &tags(&[&["x", "e"]])),
            EngagementFact::OriginalNote
        );
        assert_eq!(classify_engagement(7, &[]), EngagementFact::Reaction);
        assert_eq!(
            classify_engagement(2, &tags(&[&["e", "id"]])),
            EngagementFact::Other
        );
    }

    #[test]
    fn longform_uses_utf8_bytes_not_scalar_count() {
        let fact = classify_longform(30_023, "a🦀").expect("long-form");
        assert_eq!(fact.content_bytes, 5);
        assert!(classify_longform(1, "a🦀").is_none());
    }

    #[test]
    fn zap_multiplier_and_limit_boundaries_are_exact() {
        for (invoice, expected) in [
            ("lnbc10m1x", 1_000_000_000),
            ("lnbc10000u1x", 1_000_000_000),
            ("lnbc10000000n1x", 1_000_000_000),
            ("lnbc10000000000p1x", 1_000_000_000),
        ] {
            assert_eq!(parse_bolt11_msats(invoice), Ok(expected));
        }
        assert_eq!(
            parse_bolt11_msats("lnbc11m1x"),
            Err(ZapRejection::AmountAboveLimit)
        );
        assert_eq!(
            parse_bolt11_msats("lnbc9p1x"),
            Err(ZapRejection::ZeroAmount)
        );
        assert_eq!(
            parse_bolt11_msats("lnbc1x"),
            Err(ZapRejection::MalformedBolt11)
        );
        assert_eq!(
            parse_bolt11_msats("LNBC1u1x"),
            Err(ZapRejection::MalformedBolt11)
        );
        assert_eq!(
            parse_bolt11_msats("lnbc18446744073709551615m1x"),
            Err(ZapRejection::AmountOverflow)
        );
    }

    #[test]
    fn first_positional_tags_own_zap_semantics() {
        let values = tags(&[
            &["bolt11", "bad"],
            &["bolt11", "lnbc1u1good"],
            &["p", "bad-key"],
            &["p", &"11".repeat(32)],
            &["P", &"22".repeat(32)],
        ]);
        assert_eq!(
            classify_zap(9_735, &values),
            Some(Err(ZapRejection::MalformedBolt11))
        );

        let values = tags(&[
            &["bolt11", "lnbc1u1good"],
            &["p", "bad-key"],
            &["p", &"11".repeat(32)],
            &["P", &"22".repeat(32)],
        ]);
        let fact = classify_zap(9_735, &values)
            .expect("zap kind")
            .expect("valid amount");
        assert_eq!(fact.amount_msats, 100_000);
        assert_eq!(fact.recipient.as_deref(), Some("bad-key"));
        assert!(fact.recipient_pubkey.is_none());
        assert_eq!(fact.sender_pubkey, Some([0x22; 32]));
    }

    #[test]
    fn histogram_uses_truncated_whole_sats_at_every_boundary() {
        for (index, upper) in ZAP_HISTOGRAM_UPPER_SATS.iter().enumerate() {
            assert_eq!(zap_histogram_bucket(upper * 1_000 + 999), index as u8);
            assert_eq!(zap_histogram_bucket((upper + 1) * 1_000), index as u8 + 1);
        }
        assert_eq!(zap_histogram_bucket(1), 0);
    }

    #[test]
    fn additive_rollups_account_for_every_semantic_class() {
        let mut rollups = SemanticRollups::default();
        rollups.observe(86_401, 1, &[], "").expect("original");
        rollups
            .observe(86_402, 1, &tags(&[&["e"]]), "")
            .expect("reply");
        rollups.observe(86_403, 7, &[], "").expect("reaction");
        rollups
            .observe(86_404, 30_023, &[], "🦀")
            .expect("long-form");
        rollups
            .observe(
                86_405,
                9_735,
                &tags(&[
                    &["bolt11", "lnbc1u1x"],
                    &["p", &"11".repeat(32)],
                    &["P", "malformed"],
                ]),
                "",
            )
            .expect("accepted zap");
        rollups
            .observe(86_406, 9_735, &tags(&[&["bolt11"]]), "")
            .expect("rejected zap");

        assert_eq!(
            rollups.engagement[&86_400],
            EngagementDay {
                day_epoch: 86_400,
                original_notes: 1,
                replies: 1,
                reactions: 1,
            }
        );
        assert_eq!(rollups.longform[&86_400].articles, 1);
        assert_eq!(rollups.longform[&86_400].content_bytes, 4);
        let zaps = &rollups.zaps[&86_400];
        assert_eq!(zaps.accepted, 1);
        assert_eq!(zaps.amount_msats, 100_000);
        assert_eq!(zaps.validated_recipients, 1);
        assert_eq!(zaps.validated_senders, 0);
        assert_eq!(zaps.histogram.iter().sum::<u64>(), 1);
        assert_eq!(zaps.rejected[ZapRejection::MissingBolt11Value.ordinal()], 1);
    }

    #[test]
    fn empty_rollups_have_zero_denominators_without_synthetic_rows() {
        let rollups = SemanticRollups::default();
        assert!(rollups.engagement.is_empty());
        assert!(rollups.longform.is_empty());
        assert!(rollups.zaps.is_empty());
    }
}
