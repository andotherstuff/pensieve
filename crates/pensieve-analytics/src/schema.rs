//! Idempotent Postgres schema used by analytics planning and publication.

pub(crate) const SCHEMA_SQL: &str = concat!(
    include_str!("../../../docs/postgres/001_analytics_slice_a.sql"),
    "\n",
    include_str!("../../../docs/postgres/002_analytics_applied_objects.sql"),
);
