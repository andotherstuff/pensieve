//! Operator tool for the compact preview lookup tables (migration 017).
//!
//! Subcommands:
//! - `plan`: inspect the source, write a shard plan and print a disk estimate
//! - `backfill`: run the plan with resource limits, disk guards and checkpoints
//! - `status`: backfill progress, table sizes and freshness
//! - `validate`: compare compact and legacy reads against the source events
//! - `bench`: measure lookup latency with projections and query cache off
//!
//! See `docs/preview_compact_lookups.md` and `ops/RUNBOOK.md`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use pensieve_preview::lookups::backfill::{self, BackfillOptions, StopReason};
use pensieve_preview::lookups::bench::{self, BenchOptions};
use pensieve_preview::lookups::ops::{ConnectionArgs, GIB, LimitArgs};
use pensieve_preview::lookups::plan::{self, CheckpointDir, PlanOptions, ShardBy};
use pensieve_preview::lookups::status;
use pensieve_preview::lookups::target::Target;
use pensieve_preview::lookups::validate::{self, ValidateOptions};

/// Manage the compact ClickHouse lookup tables used by pensieve-preview.
#[derive(Parser, Debug)]
#[command(name = "preview-lookups", version)]
struct Args {
    #[command(flatten)]
    connection: ConnectionArgs,

    #[command(flatten)]
    limits: LimitArgs,

    /// Directory for plans and shard checkpoints.
    #[arg(long, default_value = "data/preview-lookups", global = true)]
    checkpoint_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect the source rows, write a shard plan and print a disk estimate.
    Plan {
        /// Targets to plan (repeatable; default all).
        #[arg(long = "target", value_enum)]
        targets: Vec<Target>,
        /// Shard dimension.
        #[arg(long, value_enum, default_value = "created-at")]
        shard_by: ShardBy,
        /// Target source rows per created-at shard.
        #[arg(long, default_value_t = 2_000_000)]
        max_rows_per_shard: u64,
        /// Hex prefix length for id-prefix shards (16^n shards).
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=3))]
        id_prefix_len: u8,
        /// Allow reading the base table without the events_by_kind projection
        /// (full-table scans; for rebuilds after the projection is removed).
        #[arg(long)]
        allow_full_scan: bool,
        /// Free-space reserve shown in the estimate, in GiB.
        #[arg(long, default_value_t = 100)]
        min_free_gib: u64,
        /// Replace an existing plan and delete its completed-shard checkpoints.
        #[arg(long)]
        discard_progress: bool,
    },
    /// Run planned shards (resumable; completed shards are skipped).
    Backfill {
        /// Targets to backfill (repeatable; default all).
        #[arg(long = "target", value_enum)]
        targets: Vec<Target>,
        /// Stop after this many minutes (between shards).
        #[arg(long)]
        max_runtime_mins: Option<u64>,
        /// Stop after this many shards per target.
        #[arg(long)]
        max_shards: Option<usize>,
        /// Seconds to pause between shards.
        #[arg(long, default_value_t = 5)]
        delay_secs: u64,
        /// Minutes that must pass after the view's creation before backfilling.
        #[arg(long, default_value_t = 10)]
        grace_mins: u64,
        /// Free space that must remain on the table's disk, in GiB.
        #[arg(long, default_value_t = 100)]
        min_free_gib: u64,
        /// Pause while the target has more active parts than this.
        #[arg(long, default_value_t = 300)]
        max_target_parts: u64,
        /// Abort a shard reading more than this multiple of its planned rows.
        #[arg(long, default_value_t = 4.0)]
        max_read_amplification: f64,
        /// After all shards, re-insert rows indexed within --gap-margin-mins of
        /// the view's creation (belt and braces; reads the target-kind slice).
        #[arg(long)]
        gap_pass: bool,
        /// Gap-closure window half-width in minutes.
        #[arg(long, default_value_t = 30)]
        gap_margin_mins: u64,
    },
    /// Show backfill progress, table sizes and freshness.
    Status,
    /// Compare compact and legacy reads against the source events.
    Validate {
        /// Targets to validate (repeatable; default all).
        #[arg(long = "target", value_enum)]
        targets: Vec<Target>,
        /// Random keys per target.
        #[arg(long, default_value_t = 500)]
        sample: usize,
        /// Hottest keys per target.
        #[arg(long, default_value_t = 50)]
        hot: usize,
        /// Sampling seed.
        #[arg(long, default_value_t = 17)]
        seed: u64,
        /// Random keys come from hash % 10000 < this.
        #[arg(long, default_value_t = 100)]
        sample_permyriad: u32,
        /// Compare only data indexed at least this many seconds ago.
        #[arg(long, default_value_t = 300)]
        cutoff_margin_secs: u64,
        /// Keys per source query.
        #[arg(long, default_value_t = 200)]
        batch: usize,
        /// Maximum legacy per-key reads for zaps and naddr.
        #[arg(long, default_value_t = 100)]
        legacy_sample: usize,
        /// Allow source reads without the events_by_kind projection.
        #[arg(long)]
        allow_full_scan: bool,
        /// Write the JSON report here.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Measure compact (and optionally legacy) lookup latency and storage.
    Bench {
        /// Measured calls per lookup.
        #[arg(long, default_value_t = 30)]
        samples: usize,
        /// Pubkeys per batch profile lookup.
        #[arg(long, default_value_t = 50)]
        batch_size: usize,
        /// Sampling seed (default: derived from the server clock).
        #[arg(long)]
        seed: Option<u64>,
        /// Also measure legacy reads with projections disabled (slow).
        #[arg(long)]
        include_legacy: bool,
        /// Legacy calls per lookup.
        #[arg(long, default_value_t = 3)]
        legacy_samples: usize,
        /// max_execution_time for legacy calls.
        #[arg(long, default_value_t = 120)]
        legacy_timeout_secs: u64,
        /// Drop the server's mark and uncompressed caches before each query.
        /// Affects every query on the server.
        #[arg(long)]
        drop_caches: bool,
        /// Latency target in milliseconds.
        #[arg(long, default_value_t = 1000)]
        target_ms: u64,
        /// Allow key sampling without the events_by_kind projection.
        #[arg(long)]
        allow_full_scan: bool,
        /// Write the JSON report here.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn targets_or_all(targets: Vec<Target>) -> Vec<Target> {
    if targets.is_empty() {
        Target::ALL.to_vec()
    } else {
        let mut seen = Vec::new();
        for t in targets {
            if !seen.contains(&t) {
                seen.push(t);
            }
        }
        seen
    }
}

fn write_report<T: serde::Serialize>(path: Option<&PathBuf>, report: &T) -> Result<()> {
    if let Some(path) = path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(report)?)
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!("report written to {}", path.display());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    match run(Args::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<ExitCode> {
    let database = args.connection.clickhouse_database.clone();
    // Tag every tool query so audits of projection use can exclude them;
    // backfill and bench override this per query with a more specific tag.
    let client = args
        .limits
        .apply(args.connection.client())
        .with_option("log_comment", "preview-lookups");

    match args.command {
        Command::Plan {
            targets,
            shard_by,
            max_rows_per_shard,
            id_prefix_len,
            allow_full_scan,
            min_free_gib,
            discard_progress,
        } => {
            let options = PlanOptions {
                shard_by,
                max_rows_per_shard,
                id_prefix_len: usize::from(id_prefix_len),
                require_projection: !allow_full_scan,
            };
            let mut peak_total = 0u64;
            for target in targets_or_all(targets) {
                let plan = plan::build_plan(&client, &database, target, &options).await?;
                CheckpointDir::new(&args.checkpoint_dir, target)
                    .save_plan(&plan, discard_progress)?;
                println!("{}\n", plan::describe_plan(&plan, min_free_gib * GIB));
                peak_total += plan.estimate.peak_bytes;
            }
            let disk =
                pensieve_preview::lookups::ops::table_disk(&client, &database, "events_local")
                    .await?;
            println!(
                "combined peak (est): {}; disk {} unreserved: {} of {}",
                pensieve_preview::lookups::ops::human_bytes(peak_total),
                disk.name,
                pensieve_preview::lookups::ops::human_bytes(disk.unreserved_space),
                pensieve_preview::lookups::ops::human_bytes(disk.total_space)
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Backfill {
            targets,
            max_runtime_mins,
            max_shards,
            delay_secs,
            grace_mins,
            min_free_gib,
            max_target_parts,
            max_read_amplification,
            gap_pass,
            gap_margin_mins,
        } => {
            let started = std::time::Instant::now();
            let max_runtime = max_runtime_mins.map(|m| Duration::from_secs(m * 60));
            let mut complete = true;
            for target in targets_or_all(targets) {
                let checkpoints = CheckpointDir::new(&args.checkpoint_dir, target);
                let Some(plan) = checkpoints.load_plan()? else {
                    bail!(
                        "no plan for {target}; run `preview-lookups plan --target {target}` first"
                    );
                };
                if plan.database != database {
                    bail!(
                        "plan for {target} targets database {}, not {database}",
                        plan.database
                    );
                }
                let options = BackfillOptions {
                    max_runtime: max_runtime.map(|m| m.saturating_sub(started.elapsed())),
                    max_shards,
                    delay_between_shards: Duration::from_secs(delay_secs),
                    grace: Duration::from_secs(grace_mins * 60),
                    skip_gap_preflight: false,
                    min_free_bytes: min_free_gib * GIB,
                    max_target_parts,
                    parts_wait_timeout: Duration::from_secs(1800),
                    max_read_amplification,
                };
                let report = backfill::run_backfill(&client, &plan, &checkpoints, &options).await?;
                println!("{target}: {}", serde_json::to_string(&report)?);
                if report.stopped != StopReason::Complete {
                    complete = false;
                    break;
                }
                if gap_pass {
                    backfill::run_gap_pass(
                        &client,
                        &plan,
                        &checkpoints,
                        Duration::from_secs(gap_margin_mins * 60),
                        &options,
                    )
                    .await?;
                }
            }
            if complete {
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!("backfill stopped before completion; re-run to resume");
                Ok(ExitCode::from(2))
            }
        }
        Command::Status => {
            let statuses = status::collect(&client, &database, &args.checkpoint_dir).await?;
            print!("{}", status::render(&statuses));
            Ok(ExitCode::SUCCESS)
        }
        Command::Validate {
            targets,
            sample,
            hot,
            seed,
            sample_permyriad,
            cutoff_margin_secs,
            batch,
            legacy_sample,
            allow_full_scan,
            out,
        } => {
            let options = ValidateOptions {
                targets: targets_or_all(targets),
                sample,
                hot,
                seed,
                sample_permyriad,
                cutoff_margin: Duration::from_secs(cutoff_margin_secs),
                batch,
                legacy_sample,
                require_projection: !allow_full_scan,
                max_examples: 20,
            };
            let report = validate::run(&client, &database, &options).await?;
            print!("{}", report.render());
            write_report(out.as_ref(), &report)?;
            Ok(if report.unexplained() == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Bench {
            samples,
            batch_size,
            seed,
            include_legacy,
            legacy_samples,
            legacy_timeout_secs,
            drop_caches,
            target_ms,
            allow_full_scan,
            out,
        } => {
            let options = BenchOptions {
                samples,
                batch_size,
                seed,
                include_legacy,
                legacy_samples,
                legacy_timeout_secs,
                drop_caches,
                target_ms,
                require_projection: !allow_full_scan,
            };
            let report = bench::run(&client, &database, &options).await?;
            print!("{}", report.render());
            write_report(out.as_ref(), &report)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
