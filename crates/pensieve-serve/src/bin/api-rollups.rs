//! Operator tool for the API rollup tables (migration 019).
//!
//! Subcommands:
//! - `plan`: fix the view boundary, size a sample shard, write a shard plan
//! - `backfill`: run the plan with resource limits, guards and checkpoints
//! - `status`: backfill progress, table sizes, freshness and the boundary
//! - `validate`: compare the rollups with a direct oracle on `events_local`
//! - `resolve`: settle a pending shard by hand when the tool cannot
//!
//! See `docs/api_rollups.md` and `ops/RUNBOOK.md`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use pensieve_serve::rollups::Target;
use pensieve_serve::rollups::backfill::{self, BackfillOptions, QuietMinutes, StopReason};
use pensieve_serve::rollups::ops::{self, ConnectionArgs, GIB, LimitArgs};
use pensieve_serve::rollups::plan::{self, CheckpointDir, PlanOptions};
use pensieve_serve::rollups::status;
use pensieve_serve::rollups::validate::{self, ValidateOptions};

/// Manage the ClickHouse rollups behind /stats/publishers and /stats/longform.
#[derive(Parser, Debug)]
#[command(name = "api-rollups", version)]
struct Args {
    #[command(flatten)]
    connection: ConnectionArgs,

    #[command(flatten)]
    limits: LimitArgs,

    /// Directory for plans and shard checkpoints.
    #[arg(long, default_value = "data/api-rollups", global = true)]
    checkpoint_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Fix the view boundary, size a sample shard and write a shard plan.
    Plan {
        /// Targets to plan (repeatable; default all).
        #[arg(long = "target", value_enum)]
        targets: Vec<Target>,
        /// Hex prefix length of the id shards (16^n shards). Each shard is one
        /// INSERT whose whole output is held as one block and written as one
        /// part: use 3 if a shard exceeds --max-memory-gib.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=4))]
        id_prefix_len: u8,
        /// Publishers: required gap between the view's creation and the first
        /// second it captured.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(2..))]
        boundary_margin_secs: u32,
        /// Free-space reserve shown in the estimate, in GiB.
        #[arg(long, default_value_t = 100)]
        min_free_gib: u64,
        /// Replace an existing plan and delete its checkpoints. For publishers
        /// only after truncating the table and recreating its view.
        #[arg(long)]
        discard_progress: bool,
    },
    /// Run planned shards (resumable; completed shards are skipped).
    Backfill {
        /// Targets to backfill (repeatable; default all).
        #[arg(long = "target", value_enum)]
        targets: Vec<Target>,
        /// Stop after this many minutes (checked before every shard).
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
        /// Do not start shards in these UTC minutes of each hour (START-END,
        /// may wrap), or `none`. The default avoids the refreshable views
        /// that rescan events_local at :00-:07.
        #[arg(long, default_value = "58-7")]
        quiet_minutes: String,
    },
    /// Show backfill progress, table sizes, freshness and the view boundary.
    Status,
    /// Compare the rollups with a direct oracle on events_local.
    Validate {
        /// Targets to validate (repeatable; default all).
        #[arg(long = "target", value_enum)]
        targets: Vec<Target>,
        /// Keys per sampling source.
        #[arg(long, default_value_t = 300)]
        sample: usize,
        /// Publishers: pubkeys sampled around the view boundary (full read of
        /// the indexed_at column; 0 to skip).
        #[arg(long, default_value_t = 200)]
        boundary_sample: usize,
        /// Publishers: half-width of the boundary window in seconds.
        #[arg(long, default_value_t = 120)]
        boundary_window_secs: u32,
        /// Sampling seed.
        #[arg(long, default_value_t = 19)]
        seed: u64,
        /// Rollup-side keys come from hash % 10000 < this.
        #[arg(long, default_value_t = 100)]
        sample_permyriad: u32,
        /// Compare only data indexed at least this many seconds ago.
        #[arg(long, default_value_t = 300)]
        cutoff_margin_secs: u64,
        /// Keys per oracle query.
        #[arg(long, default_value_t = 100)]
        batch: usize,
        /// Publishers: skip pubkeys with more events than this in the window.
        #[arg(long, default_value_t = 5000)]
        max_key_events: u64,
        /// Write the JSON report here.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Settle a pending shard by hand (after checking system.part_log or the
    /// table): record it as done or let it run again.
    Resolve {
        /// Target of the pending shard.
        #[arg(long, value_enum)]
        target: Target,
        /// Shard index.
        #[arg(long)]
        shard: usize,
        /// The shard's INSERT committed.
        #[arg(
            long,
            conflicts_with = "not_committed",
            required_unless_present = "not_committed"
        )]
        committed: bool,
        /// The shard's INSERT did not commit.
        #[arg(long)]
        not_committed: bool,
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
    // Tag every tool query; backfill shards override this with their own tag.
    let client = args
        .limits
        .apply(args.connection.client())
        .with_option("log_comment", "api-rollups");

    match args.command {
        Command::Plan {
            targets,
            id_prefix_len,
            boundary_margin_secs,
            min_free_gib,
            discard_progress,
        } => {
            let options = PlanOptions {
                id_prefix_len: usize::from(id_prefix_len),
                boundary_margin_secs,
            };
            let mut peak_total = 0u64;
            for target in targets_or_all(targets) {
                let checkpoints = CheckpointDir::new(&args.checkpoint_dir, target);
                // Refuse before doing any work if progress would be lost.
                if !discard_progress
                    && (!checkpoints.completed_shards()?.is_empty()
                        || !checkpoints.pending_shards()?.is_empty())
                {
                    bail!(
                        "{} has backfill progress; pass --discard-progress to replan",
                        checkpoints.root().display()
                    );
                }
                let plan = plan::build_plan(&client, &database, target, &options).await?;
                checkpoints.save_plan(&plan, discard_progress)?;
                println!("{}\n", plan::describe_plan(&plan, min_free_gib * GIB));
                peak_total += plan.estimate.peak_bytes;
            }
            let disk = ops::table_disk(&client, &database, "events_local").await?;
            println!(
                "combined peak (est): {}; disk {} unreserved: {} of {}",
                ops::human_bytes(peak_total),
                disk.name,
                ops::human_bytes(disk.unreserved_space),
                ops::human_bytes(disk.total_space)
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
            quiet_minutes,
        } => {
            let quiet_minutes = match quiet_minutes.as_str() {
                "none" => None,
                window => Some(
                    window
                        .parse::<QuietMinutes>()
                        .map_err(anyhow::Error::msg)
                        .context("--quiet-minutes")?,
                ),
            };
            let started = std::time::Instant::now();
            let max_runtime = max_runtime_mins.map(|m| Duration::from_secs(m * 60));
            for target in targets_or_all(targets) {
                let checkpoints = CheckpointDir::new(&args.checkpoint_dir, target);
                let Some(plan) = checkpoints.load_plan()? else {
                    bail!("no plan for {target}; run `api-rollups plan --target {target}` first");
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
                    min_free_bytes: min_free_gib * GIB,
                    max_target_parts,
                    parts_wait_timeout: Duration::from_secs(1800),
                    quiet_minutes,
                };
                let report = backfill::run_backfill(&client, &plan, &checkpoints, &options).await?;
                println!("{target}: {}", serde_json::to_string(&report)?);
                if report.stopped != StopReason::Complete {
                    eprintln!("backfill stopped before completion; re-run to resume");
                    return Ok(ExitCode::from(2));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => {
            let statuses = status::collect(&client, &database, &args.checkpoint_dir).await?;
            print!("{}", status::render(&statuses));
            Ok(ExitCode::SUCCESS)
        }
        Command::Validate {
            targets,
            sample,
            boundary_sample,
            boundary_window_secs,
            seed,
            sample_permyriad,
            cutoff_margin_secs,
            batch,
            max_key_events,
            out,
        } => {
            let targets = targets_or_all(targets);
            for target in &targets {
                let checkpoints = CheckpointDir::new(&args.checkpoint_dir, *target);
                if !backfill::is_complete(&checkpoints)? {
                    eprintln!(
                        "warning: {target} backfill is not complete (or not planned here); \
                         missing rows are expected"
                    );
                }
            }
            let publisher_plan =
                CheckpointDir::new(&args.checkpoint_dir, Target::Publishers).load_plan()?;
            let options = ValidateOptions {
                targets,
                sample,
                boundary_sample,
                boundary_window_secs,
                seed,
                sample_permyriad,
                cutoff_margin: Duration::from_secs(cutoff_margin_secs),
                batch,
                max_key_events,
                max_examples: 20,
            };
            let report =
                validate::run(&client, &database, &options, publisher_plan.as_ref()).await?;
            print!("{}", report.render());
            if let Some(path) = out {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, serde_json::to_vec_pretty(&report)?)
                    .with_context(|| format!("writing {}", path.display()))?;
                eprintln!("report written to {}", path.display());
            }
            Ok(if report.passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Resolve {
            target,
            shard,
            committed,
            not_committed: _,
        } => {
            let checkpoints = CheckpointDir::new(&args.checkpoint_dir, target);
            let now = ops::server_now(&client).await?;
            backfill::resolve_by_hand(&checkpoints, shard, committed, now)?;
            println!(
                "{target} shard {shard} recorded as {}",
                if committed {
                    "committed"
                } else {
                    "not committed (it will run again)"
                }
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}
