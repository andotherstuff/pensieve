//! Idempotent Postgres schema used by analytics planning and publication.

pub(crate) const SCHEMA_SQL: &str = concat!(
    include_str!("../../../docs/postgres/001_analytics_slice_a.sql"),
    "\n",
    include_str!("../../../docs/postgres/002_analytics_applied_objects.sql"),
    "\n",
    include_str!("../../../docs/postgres/003_analytics_slice_b_identity.sql"),
    "\n",
    include_str!("../../../docs/postgres/004_analytics_slice_b_activity.sql"),
    "\n",
    include_str!("../../../docs/postgres/005_analytics_slice_b_cohort_retention.sql"),
    "\n",
    include_str!("../../../docs/postgres/006_analytics_flexible_distinct.sql"),
);
