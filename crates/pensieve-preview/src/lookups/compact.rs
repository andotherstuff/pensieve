//! Reads against the compact lookup tables created by
//! `docs/migrations/prepared/017_preview_compact_lookups.sql`.
//!
//! Every query collapses duplicate rows or aggregate states itself
//! (`argMinMerge` / `GROUP BY receipt_id`), so results are the same before and
//! after background merges. None of these reads touch the `events_by_kind`
//! projection: lookups are primary-key range reads on small tables, and naddr
//! rows are then fetched from `events_local` by event id.

use std::collections::HashMap;

use clickhouse::Client;
use serde::Deserialize;

use crate::error::PreviewError;
use crate::query::{EventRow, ProfileRow, ZapTotals, display_names_from_profiles};

/// Whether `addressable_latest` covers a kind (see
/// [`ADDRESSABLE_KINDS`](super::target::ADDRESSABLE_KINDS)). Other naddr kinds
/// use the legacy query.
pub fn addressable_kind_covered(kind: u16) -> bool {
    super::target::ADDRESSABLE_KINDS
        .binary_search(&kind)
        .is_ok()
}

/// Fetch the latest profile (kind 0) for a pubkey from `profile_latest`.
pub async fn fetch_profile(
    client: &Client,
    pubkey: &str,
) -> Result<Option<ProfileRow>, PreviewError> {
    let row = client
        .query(
            "SELECT pubkey, tupleElement(winner, 1) AS event_id, \
             tupleElement(winner, 2) AS content \
             FROM ( \
                 SELECT pubkey, argMinMerge(latest) AS winner \
                 FROM profile_latest \
                 WHERE pubkey = ? \
                 GROUP BY pubkey \
             )",
        )
        .bind(pubkey)
        .fetch_optional::<ProfileRow>()
        .await?;

    Ok(row)
}

/// Batch-fetch latest profiles for a set of pubkeys from `profile_latest`.
///
/// Pubkeys without a profile are omitted.
pub async fn fetch_profiles(
    client: &Client,
    pubkeys: &[String],
) -> Result<HashMap<String, ProfileRow>, PreviewError> {
    if pubkeys.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders: Vec<&str> = pubkeys.iter().map(|_| "?").collect();
    let query_str = format!(
        "SELECT pubkey, tupleElement(winner, 1) AS event_id, \
         tupleElement(winner, 2) AS content \
         FROM ( \
             SELECT pubkey, argMinMerge(latest) AS winner \
             FROM profile_latest \
             WHERE pubkey IN ({}) \
             GROUP BY pubkey \
         )",
        placeholders.join(", ")
    );

    let mut query = client.query(&query_str);
    for pk in pubkeys {
        query = query.bind(pk);
    }

    let rows = query.fetch_all::<ProfileRow>().await?;
    Ok(rows.into_iter().map(|r| (r.pubkey.clone(), r)).collect())
}

/// Batch-fetch display names for a set of pubkeys from `profile_latest`.
pub async fn fetch_display_names(
    client: &Client,
    pubkeys: &[String],
) -> Result<HashMap<String, String>, PreviewError> {
    let profiles = fetch_profiles(client, pubkeys).await?;
    Ok(display_names_from_profiles(
        profiles.into_values().map(|row| (row.pubkey, row.content)),
    ))
}

/// Fetch the latest replaceable/addressable event for a coordinate via the
/// `addressable_latest` pointer and an `events_local` primary-key read.
///
/// Callers must check [`addressable_kind_covered`] first.
pub async fn fetch_replaceable(
    client: &Client,
    kind: u16,
    pubkey: &str,
    d_tag: &str,
) -> Result<Option<EventRow>, PreviewError> {
    let row = client
        .query(
            "SELECT id, pubkey, created_at, kind, content, tags, \
             d_tag, title, thumbnail, video_url \
             FROM events_local \
             WHERE id IN ( \
                 SELECT argMinMerge(latest) \
                 FROM addressable_latest \
                 WHERE pubkey = ? AND kind = ? AND d_tag = ? \
             ) \
             LIMIT 1",
        )
        .bind(pubkey)
        .bind(kind)
        .bind(d_tag)
        .fetch_optional::<EventRow>()
        .await?;

    Ok(row)
}

/// Fetch zap totals for a target event from `zap_receipts_by_target`.
///
/// Rows are collapsed per receipt id before summing, so replayed receipts are
/// counted once even before merges, and the highest `parser_version` wins.
/// Only receipts the parser accepted are counted (the migration 009 rule).
pub async fn fetch_zap_totals(client: &Client, event_id: &str) -> Result<ZapTotals, PreviewError> {
    #[derive(Debug, Deserialize, clickhouse::Row)]
    struct Row {
        total_msats: u64,
        zap_count: u64,
    }

    let row = client
        .query(
            "SELECT sumIf(receipt_msats, receipt_parsed = 1) AS total_msats, \
             countIf(receipt_parsed = 1) AS zap_count \
             FROM ( \
                 SELECT receipt_id, \
                 argMax(amount_msats, parser_version) AS receipt_msats, \
                 argMax(amount_parsed, parser_version) AS receipt_parsed \
                 FROM zap_receipts_by_target \
                 WHERE target_event_id = ? \
                 GROUP BY receipt_id \
             )",
        )
        .bind(event_id)
        .fetch_optional::<Row>()
        .await?;

    Ok(row.map_or_else(ZapTotals::default, |r| ZapTotals {
        total_msats: r.total_msats,
        zap_count: r.zap_count,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressable_coverage_is_the_previewable_list() {
        for kind in [30023, 30024, 34235, 34236, 30311, 30818, 34550, 39089] {
            assert!(addressable_kind_covered(kind), "{kind}");
        }
        // Bulk machine kinds, replaceable lists and non-addressable kinds fall
        // back to the legacy query.
        for kind in [0, 1, 10002, 30000, 30078, 30304, 30382, 30383, 40000] {
            assert!(!addressable_kind_covered(kind), "{kind}");
        }
    }
}
