//! Plan bounded analytics work without reading Parquet objects.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_analytics::plan_catalog_delta;
use pensieve_lake::read_catalog_snapshot;
use postgres::{Config as PostgresConfig, NoTls};

#[derive(Debug, Parser)]
#[command(about = "Compare an active-file snapshot with applied analytics objects")]
struct Args {
    /// Canonically encoded active-raw snapshot JSON.
    #[arg(long)]
    catalog: PathBuf,
    /// Postgres connection string for the analytics serving database.
    #[arg(long, env = "DATABASE_URL")]
    postgres_url: String,
    /// Postgres password supplied separately from the connection string.
    #[arg(long, env = "POSTGRES_ANALYTICS_PASSWORD")]
    postgres_password: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("analytics planning failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let snapshot = read_catalog_snapshot(&args.catalog).context("read active-file snapshot")?;
    let mut config: PostgresConfig = args
        .postgres_url
        .parse()
        .context("parse Postgres connection")?;
    if let Some(password) = args.postgres_password {
        config.password(password);
    }
    let mut client = config
        .connect(NoTls)
        .context("connect to Postgres without TLS")?;
    let plan = plan_catalog_delta(&mut client, &snapshot).context("plan catalog delta")?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}
