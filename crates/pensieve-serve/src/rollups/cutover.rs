//! `api-rollups cutover`: phase 2 of migration 020.
//!
//! Runs `docs/migrations/prepared/020_incremental_derived_views_cutover.sql`
//! one statement at a time, after checking that it is safe:
//!
//! - phase 1 is applied (the three views and `daily_user_stats.last_indexed_at`
//!   exist);
//! - the user-flags, relay-lists and kinds backfills are complete (checkpoint
//!   directory), so the tables the switched views read are populated;
//! - nothing outside this tool read `kinds_stats_mv` recently, i.e. the API
//!   release that reads `kind_totals` is live (the file drops
//!   `kinds_stats_mv`);
//! - it is not inside the quiet minutes: creating refreshable views runs them
//!   at once.
//!
//! The statements are idempotent, so a cutover that stopped half-way is
//! finished by running it again. The time is recorded in
//! `<checkpoint_dir>/cutover-020.json`; `validate` uses it to check rows
//! written around the switch.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clickhouse::Client;
use serde::{Deserialize, Serialize};

use super::Target;
use super::backfill::{self, QuietMinutes};
use super::derived::OLD_KINDS_VIEW;
use super::ops;
use super::plan::CheckpointDir;

/// The cutover SQL (phase 2 of migration 020).
pub const CUTOVER_SQL: &str =
    include_str!("../../../../docs/migrations/prepared/020_incremental_derived_views_cutover.sql");

/// Record of a completed cutover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutoverRecord {
    /// Database the cutover ran in.
    pub database: String,
    /// Server time before the first statement.
    pub started_at: u32,
    /// Server time after the last statement: every insert that started after
    /// this used the new `daily_user_stats_mv` query.
    pub finished_at: u32,
    /// Statements executed.
    pub statements: usize,
}

/// Cutover options.
#[derive(Debug, Clone)]
pub struct CutoverOptions {
    /// Print the statements and run the checks, but change nothing.
    pub dry_run: bool,
    /// Refuse while a query outside this tool read `kinds_stats_mv` within
    /// this many minutes (0 disables the check).
    pub reader_window_mins: u32,
    /// Do not run inside these minutes of the hour (UTC).
    pub quiet_minutes: Option<QuietMinutes>,
}

impl Default for CutoverOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            reader_window_mins: 15,
            quiet_minutes: Some(QuietMinutes { start: 58, end: 7 }),
        }
    }
}

/// Path of the cutover record.
pub fn record_path(checkpoint_dir: &Path) -> PathBuf {
    checkpoint_dir.join("cutover-020.json")
}

/// The recorded cutover, if any.
pub fn load_record(checkpoint_dir: &Path) -> Result<Option<CutoverRecord>> {
    let path = record_path(checkpoint_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Split SQL into statements, drop comments, point `nostr` at `database` and
/// leave out the trailing verification `SELECT`s.
pub fn statements(sql: &str, database: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in sql.lines() {
        let code = line.find("--").map_or(line, |i| &line[..i]);
        if code.trim().is_empty() {
            continue;
        }
        current.push_str(code);
        current.push('\n');
        if code.trim_end().ends_with(';') {
            let statement = current
                .trim()
                .trim_end_matches(';')
                .replace("nostr.", &format!("{database}."))
                .replace("database = 'nostr'", &format!("database = '{database}'"));
            if !statement.starts_with("SELECT") {
                out.push(statement);
            }
            current.clear();
        }
    }
    out
}

/// Check that the cutover is safe now; returns what was checked.
pub async fn preflight(
    client: &Client,
    database: &str,
    checkpoint_dir: &Path,
    options: &CutoverOptions,
) -> Result<Vec<String>> {
    let mut checked = Vec::new();
    for target in Target::MIGRATION_020 {
        ops::require_view(client, database, target).await?;
        let checkpoints = CheckpointDir::new(checkpoint_dir, target);
        let Some(plan) = checkpoints.load_plan()? else {
            bail!(
                "no {target} plan in {}; plan and backfill first",
                checkpoint_dir.display()
            );
        };
        if plan.database != database {
            bail!(
                "the {target} plan is for database {}, not {database}",
                plan.database
            );
        }
        if !backfill::is_complete(&checkpoints)? {
            bail!("the {target} backfill is not complete; finish it (and validate) first");
        }
        checked.push(format!("{target}: view exists, backfill complete"));
    }
    let has_column: u64 = client
        .query(
            "SELECT count() FROM system.columns \
             WHERE database = ? AND table = 'daily_user_stats' AND name = 'last_indexed_at'",
        )
        .bind(database)
        .fetch_one()
        .await
        .context("reading system.columns")?;
    if has_column == 0 {
        bail!(
            "{database}.daily_user_stats has no last_indexed_at column; apply \
             docs/migrations/prepared/020_incremental_derived_views.sql first"
        );
    }
    checked.push("daily_user_stats.last_indexed_at exists".to_string());

    if options.reader_window_mins > 0
        && ops::table_created_at(client, database, OLD_KINDS_VIEW)
            .await?
            .is_some()
    {
        ops::flush_logs(client).await;
        let readers: u64 = client
            .query(
                "SELECT count() FROM system.query_log \
                 WHERE event_date >= yesterday() \
                 AND event_time > now() - toIntervalMinute(?) \
                 AND type = 'QueryFinish' AND query_kind = 'Select' \
                 AND has(tables, concat(?, '.', ?)) \
                 AND NOT startsWith(log_comment, 'api-rollups')",
            )
            .bind(options.reader_window_mins)
            .bind(database)
            .bind(OLD_KINDS_VIEW)
            .fetch_one()
            .await
            .context("reading system.query_log")?;
        if readers > 0 {
            bail!(
                "{readers} query(ies) read {database}.{OLD_KINDS_VIEW} in the last {} minutes: \
                 the pre-020 API is still live. Deploy the API release that reads kind_totals \
                 first (ops/RUNBOOK.md), wait for its cache to expire, then retry",
                options.reader_window_mins
            );
        }
        checked.push(format!(
            "no reads of {OLD_KINDS_VIEW} in the last {} minutes",
            options.reader_window_mins
        ));
    }

    if let Some(quiet) = options.quiet_minutes {
        let wait = quiet.wait_secs(ops::server_now(client).await?);
        if wait > 0 {
            bail!(
                "inside the quiet minutes {quiet} (refreshable views rescan events_local); \
                 retry in {wait}s"
            );
        }
    }
    Ok(checked)
}

/// Run the cutover. With `dry_run`, only the checks run.
pub async fn run(
    client: &Client,
    database: &str,
    checkpoint_dir: &Path,
    options: &CutoverOptions,
) -> Result<Option<CutoverRecord>> {
    let checked = preflight(client, database, checkpoint_dir, options).await?;
    for line in &checked {
        tracing::info!(check = %line, "cutover preflight");
    }
    let statements = statements(CUTOVER_SQL, database);
    if options.dry_run {
        for statement in &statements {
            println!("{statement};\n");
        }
        return Ok(None);
    }
    let client = client
        .clone()
        .with_option("allow_experimental_refreshable_materialized_view", "1");
    let started_at = ops::server_now(&client).await?;
    for (i, statement) in statements.iter().enumerate() {
        let head = statement.lines().next().unwrap_or_default();
        tracing::info!(
            step = i + 1,
            of = statements.len(),
            statement = head,
            "cutover"
        );
        client.query(statement).execute().await.with_context(|| {
            format!(
                "cutover statement {} of {} failed: {head}. The statements are idempotent: \
                     fix the cause and re-run `cutover`, or roll back with \
                     docs/migrations/rollback/020_undo_derived_views_cutover.sql",
                i + 1,
                statements.len()
            )
        })?;
    }
    let record = CutoverRecord {
        database: database.to_string(),
        started_at,
        finished_at: ops::server_now(&client).await?,
        statements: statements.len(),
    };
    std::fs::create_dir_all(checkpoint_dir)
        .with_context(|| format!("creating {}", checkpoint_dir.display()))?;
    let path = record_path(checkpoint_dir);
    std::fs::write(&path, serde_json::to_vec_pretty(&record)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutover_statements_are_bind_free_and_ordered() {
        let statements = statements(CUTOVER_SQL, "scratch");
        assert_eq!(statements.len(), 10, "{statements:#?}");
        for s in &statements {
            assert!(!s.contains('?'), "{s}");
            assert!(!s.contains("nostr."), "{s}");
        }
        let position = |needle: &str| {
            statements
                .iter()
                .position(|s| s.contains(needle))
                .unwrap_or_else(|| panic!("no statement contains {needle}"))
        };
        // The refreshes read the flags before the insert path stops writing them.
        assert!(
            position("CREATE MATERIALIZED VIEW scratch.active_users_monthly_mv")
                < position("ALTER TABLE scratch.daily_user_stats_mv MODIFY QUERY")
        );
        assert!(position("DROP VIEW IF EXISTS scratch.kinds_stats_mv") == statements.len() - 1);
    }

    #[test]
    fn record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_record(dir.path()).unwrap().is_none());
        let record = CutoverRecord {
            database: "db".into(),
            started_at: 1,
            finished_at: 2,
            statements: 9,
        };
        std::fs::write(
            record_path(dir.path()),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        assert_eq!(load_record(dir.path()).unwrap(), Some(record));
    }
}
