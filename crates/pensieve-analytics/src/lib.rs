//! Rebuildable analytics products derived from canonical Parquet snapshots.
//!
//! DuckDB performs large object scans and exact aggregation. Postgres receives
//! only small serving relations, all keyed by one immutable analytics run.

mod build;
mod error;
mod input;
mod publish;

pub use build::{
    AnalyticsBuild, BuildConfig, BuildSummary, EventDaily, EventDailyKind, KindAllTime, Overview,
    QUERY_VERSION,
};
pub use error::{Error, Result};
pub use input::{ObjectLocation, ResolvedSnapshot, resolve_snapshot};
pub use publish::{PublishOutcome, publish};
