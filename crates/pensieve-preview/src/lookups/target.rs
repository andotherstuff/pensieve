//! The three compact lookup tables and the SQL that fills them.
//!
//! The backfill `SELECT` bodies here must stay equivalent to the
//! materialized views in `docs/migrations/prepared/017_preview_compact_lookups.sql`.
//! The integration tests insert the same events through both paths and assert
//! identical reads.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A compact lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// `profile_latest`: latest kind 0 per pubkey.
    Profiles,
    /// `zap_receipts_by_target`: one row per (target event, zap receipt).
    Zaps,
    /// `addressable_latest`: latest event id per (pubkey, kind, d_tag).
    Addressable,
}

/// Addressable kinds covered by `addressable_latest`, sorted: shareable
/// content that nstr.to previews via naddr (articles, video, live events,
/// wiki, publications, listings, calendar, communities, marketplace,
/// curation sets, git repos, music, starter packs). Bulk machine kinds such
/// as 30382/30304/30078 and relay lists are excluded; their naddr previews use
/// the legacy query. Must match the view's `WHERE` in migration 017.
pub const ADDRESSABLE_KINDS: [u16; 20] = [
    30004, 30005, 30017, 30018, 30023, 30024, 30040, 30041, 30311, 30402, 30403, 30617, 30818,
    31337, 31922, 31923, 34235, 34236, 34550, 39089,
];

/// SQL form of [`ADDRESSABLE_KINDS`].
const ADDRESSABLE_KINDS_PREDICATE: &str = "kind IN (30004, 30005, 30017, 30018, 30023, 30024, 30040, 30041, 30311, 30402, 30403, 30617, 30818, 31337, 31922, 31923, 34235, 34236, 34550, 39089)";

/// First `d` tag value, identical to the `events_local.d_tag` MATERIALIZED
/// expression.
pub const D_TAG_EXPR: &str = "arrayElement(arrayFilter(t -> t[1] = 'd', tags), 1)[2]";

/// Distinct non-empty `e` tag values of an event.
pub const ZAP_TARGETS_EXPR: &str = "arrayDistinct(arrayMap(t -> t[2], \
     arrayFilter(t -> length(t) >= 2 AND t[1] = 'e' AND t[2] != '', tags)))";

/// Winner key for replaceable events: newest `created_at`, then lowest id.
const WINNER_KEY_EXPR: &str = "CAST((-toInt64(created_at), id), 'Tuple(Int64, String)')";

impl Target {
    /// Every target, in rollout order.
    pub const ALL: [Self; 3] = [Self::Profiles, Self::Zaps, Self::Addressable];

    /// Short stable name used in paths and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Profiles => "profiles",
            Self::Zaps => "zaps",
            Self::Addressable => "addressable",
        }
    }

    /// Target table name.
    pub fn table(self) -> &'static str {
        match self {
            Self::Profiles => "profile_latest",
            Self::Zaps => "zap_receipts_by_target",
            Self::Addressable => "addressable_latest",
        }
    }

    /// Materialized view feeding the target from `events_local` inserts.
    pub fn materialized_view(self) -> &'static str {
        match self {
            Self::Profiles => "profile_latest_mv",
            Self::Zaps => "zap_receipts_by_target_mv",
            Self::Addressable => "addressable_latest_mv",
        }
    }

    /// Source kind filter on `events_local`.
    pub fn kind_predicate(self) -> &'static str {
        match self {
            Self::Profiles => "kind = 0",
            Self::Zaps => "kind = 9735",
            Self::Addressable => ADDRESSABLE_KINDS_PREDICATE,
        }
    }

    /// `INSERT ... SELECT` that writes the target rows for source events
    /// matching `range_predicate` (a trusted, internally generated SQL
    /// predicate such as a `created_at` window).
    pub fn backfill_sql(self, database: &str, range_predicate: &str) -> String {
        let kinds = self.kind_predicate();
        match self {
            Self::Profiles => format!(
                "INSERT INTO {database}.profile_latest (pubkey, latest, last_indexed_at) \
                 SELECT pubkey, \
                 argMinState(CAST((id, content, created_at), 'Tuple(String, String, DateTime)'), \
                 {WINNER_KEY_EXPR}) AS latest, \
                 max(indexed_at) AS last_indexed_at \
                 FROM {database}.events_local \
                 WHERE {kinds} AND ({range_predicate}) \
                 GROUP BY pubkey"
            ),
            Self::Zaps => format!(
                "INSERT INTO {database}.zap_receipts_by_target \
                 (target_event_id, receipt_id, amount_msats, amount_parsed, parser_version, \
                 receipt_created_at, indexed_at) \
                 WITH \
                 (arrayFilter(t -> t[1] = 'bolt11', tags)[1])[2] AS first_bolt11, \
                 multiIf( \
                 extract(first_bolt11, '^lnbc([0-9]+)m1') != '', \
                 toUInt64OrZero(extract(first_bolt11, '^lnbc([0-9]+)m1')) * 100000000, \
                 extract(first_bolt11, '^lnbc([0-9]+)u1') != '', \
                 toUInt64OrZero(extract(first_bolt11, '^lnbc([0-9]+)u1')) * 100000, \
                 extract(first_bolt11, '^lnbc([0-9]+)n1') != '', \
                 toUInt64OrZero(extract(first_bolt11, '^lnbc([0-9]+)n1')) * 100, \
                 extract(first_bolt11, '^lnbc([0-9]+)p1') != '', \
                 intDiv(toUInt64OrZero(extract(first_bolt11, '^lnbc([0-9]+)p1')), 10), \
                 toUInt64(0)) AS raw_amount_msats \
                 SELECT \
                 target_event_id, \
                 id AS receipt_id, \
                 if(raw_amount_msats > toUInt64(1000000000), toUInt64(0), raw_amount_msats) AS amount_msats, \
                 toUInt8(arrayExists(t -> length(t) >= 2 AND t[1] = 'bolt11' \
                 AND match(t[2], '^lnbc[0-9]+[munp]1'), tags)) AS amount_parsed, \
                 toUInt32(1) AS parser_version, \
                 created_at AS receipt_created_at, \
                 indexed_at \
                 FROM ( \
                 SELECT id, created_at, tags, indexed_at \
                 FROM {database}.events_local \
                 WHERE {kinds} AND ({range_predicate}) \
                 ) \
                 ARRAY JOIN {ZAP_TARGETS_EXPR} AS target_event_id"
            ),
            Self::Addressable => format!(
                "INSERT INTO {database}.addressable_latest \
                 (pubkey, kind, d_tag, latest, last_indexed_at) \
                 SELECT pubkey, kind, {D_TAG_EXPR} AS d_tag, \
                 argMinState(id, {WINNER_KEY_EXPR}) AS latest, \
                 max(indexed_at) AS last_indexed_at \
                 FROM {database}.events_local \
                 WHERE {kinds} AND ({range_predicate}) \
                 GROUP BY pubkey, kind, d_tag"
            ),
        }
    }

    /// Per-hour source row histogram used to plan `created_at` shards.
    pub fn hour_histogram_sql(self, database: &str) -> String {
        format!(
            "SELECT toUInt32(toStartOfHour(created_at)) AS hour, count() AS rows \
             FROM {database}.events_local \
             WHERE {} \
             GROUP BY hour ORDER BY hour",
            self.kind_predicate()
        )
    }

    /// Source statistics for disk estimation. Returns one row with
    /// `source_rows`, `distinct_keys`, `output_rows` and `avg_payload_bytes`.
    pub fn source_stats_sql(self, database: &str) -> String {
        let kinds = self.kind_predicate();
        match self {
            Self::Profiles => format!(
                "SELECT count() AS source_rows, uniq(pubkey) AS distinct_keys, \
                 distinct_keys AS output_rows, \
                 ifNotFinite(avg(length(content)), 0) AS avg_payload_bytes \
                 FROM {database}.events_local WHERE {kinds}"
            ),
            Self::Zaps => format!(
                "SELECT count() AS source_rows, uniq(id) AS distinct_keys, \
                 sum(length({ZAP_TARGETS_EXPR})) AS output_rows, \
                 toFloat64(0) AS avg_payload_bytes \
                 FROM {database}.events_local WHERE {kinds}"
            ),
            Self::Addressable => format!(
                "SELECT count() AS source_rows, \
                 uniq(pubkey, kind, {D_TAG_EXPR}) AS distinct_keys, \
                 distinct_keys AS output_rows, \
                 ifNotFinite(avg(length({D_TAG_EXPR})), 0) AS avg_payload_bytes \
                 FROM {database}.events_local WHERE {kinds}"
            ),
        }
    }

    /// Estimated stored bytes per output row, before compression, split into
    /// (hex/fixed bytes, free-text payload bytes).
    pub fn row_bytes(self, avg_payload_bytes: f64) -> (f64, f64) {
        match self {
            // pubkey + (id, content, created_at) payload + (-created_at, id) key
            // + last_indexed_at + length prefixes.
            Self::Profiles => (
                64.0 + 64.0 + 4.0 + 8.0 + 64.0 + 4.0 + 8.0,
                avg_payload_bytes,
            ),
            // target + receipt + amount + parsed + version + 2 timestamps.
            Self::Zaps => (64.0 + 64.0 + 8.0 + 1.0 + 4.0 + 4.0 + 4.0 + 4.0, 0.0),
            // pubkey + kind + id payload + (-created_at, id) key + last_indexed_at.
            Self::Addressable => (
                64.0 + 2.0 + 64.0 + 8.0 + 64.0 + 4.0 + 8.0,
                avg_payload_bytes,
            ),
        }
    }

    /// Whether the target aggregates many source rows into one row per key
    /// (so a shard writes at most one state per key).
    pub fn aggregates_per_key(self) -> bool {
        !matches!(self, Self::Zaps)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backfill_sql_is_scoped_and_bind_free() {
        for target in Target::ALL {
            let sql = target.backfill_sql("scratch", "created_at >= toDateTime(5)");
            assert!(sql.starts_with(&format!("INSERT INTO scratch.{}", target.table())));
            assert!(sql.contains("FROM scratch.events_local"));
            assert!(sql.contains(target.kind_predicate()));
            assert!(sql.contains("(created_at >= toDateTime(5))"));
            assert!(!sql.contains("nostr."));
            // `?` is the clickhouse crate's bind placeholder.
            assert!(
                !sql.contains('?'),
                "{target} backfill SQL must not contain '?'"
            );
        }
    }

    #[test]
    fn source_sql_is_bind_free() {
        for target in Target::ALL {
            assert!(!target.hour_histogram_sql("db").contains('?'));
            assert!(!target.source_stats_sql("db").contains('?'));
        }
    }

    #[test]
    fn addressable_kinds_match_predicate_and_migration() {
        let mut sorted = ADDRESSABLE_KINDS;
        sorted.sort_unstable();
        assert_eq!(sorted, ADDRESSABLE_KINDS, "keep ADDRESSABLE_KINDS sorted");
        let expected = format!(
            "kind IN ({})",
            ADDRESSABLE_KINDS
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert_eq!(ADDRESSABLE_KINDS_PREDICATE, expected);
        let migration =
            include_str!("../../../../docs/migrations/prepared/017_preview_compact_lookups.sql");
        assert!(
            migration.contains(&format!("WHERE {expected}\nGROUP BY pubkey, kind, d_tag;")),
            "migration 017 addressable view must filter on exactly {expected}"
        );
    }

    #[test]
    fn names_are_distinct() {
        let mut names: Vec<_> = Target::ALL.iter().map(|t| t.table()).collect();
        names.dedup();
        assert_eq!(names.len(), 3);
    }
}
