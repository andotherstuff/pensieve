//! Repair utility for corrupted RocksDB dedupe index.
//!
//! Attempts to repair a corrupted RocksDB database, or copy valid data
//! to a new database.
//!
//! # Usage
//!
//! ```bash
//! # Attempt in-place repair
//! repair-dedupe --path /data/rocksdb --repair
//!
//! # Copy valid data to new database
//! repair-dedupe --path /data/rocksdb --copy-to /data/rocksdb-new
//!
//! # Just check if it can be opened
//! repair-dedupe --path /data/rocksdb --check
//! ```

use anyhow::{Context, Result};
use clap::Parser;
use rocksdb::{DB, DBWithThreadMode, MultiThreaded, Options, WriteBatch};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "repair-dedupe")]
#[command(about = "Repair or recover a corrupted RocksDB dedupe index")]
struct Args {
    /// Path to the RocksDB database
    #[arg(long)]
    path: PathBuf,

    /// Attempt in-place repair using RocksDB's repair function
    #[arg(long)]
    repair: bool,

    /// Copy valid data to a new database at this path
    #[arg(long)]
    copy_to: Option<PathBuf>,

    /// Just check if the database can be opened
    #[arg(long)]
    check: bool,

    /// Show statistics about the database
    #[arg(long)]
    stats: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let args = Args::parse();

    if args.repair {
        repair_database(&args.path)?;
    }

    if args.check || args.stats {
        check_database(&args.path, args.stats)?;
    }

    if let Some(ref dest) = args.copy_to {
        copy_database(&args.path, dest)?;
    }

    if !args.repair && !args.check && !args.stats && args.copy_to.is_none() {
        println!("No action specified. Use --repair, --check, --stats, or --copy-to");
        println!("Run with --help for usage information");
    }

    Ok(())
}

fn repair_database(path: &PathBuf) -> Result<()> {
    println!("Attempting to repair database at: {}", path.display());

    let mut opts = Options::default();
    opts.create_if_missing(false);

    // RocksDB's repair function
    DB::repair(&opts, path).context("RocksDB repair failed")?;

    println!("✓ Repair completed successfully");
    println!("  Try opening the database with --check to verify");

    Ok(())
}

fn check_database(path: &PathBuf, show_stats: bool) -> Result<()> {
    println!("Checking database at: {}", path.display());

    let mut opts = Options::default();
    opts.create_if_missing(false);

    // Configure for read-only check
    let mut block_opts = rocksdb::BlockBasedOptions::default();
    block_opts.set_bloom_filter(10.0, false);
    opts.set_block_based_table_factory(&block_opts);

    let db =
        DBWithThreadMode::<MultiThreaded>::open(&opts, path).context("Failed to open database")?;

    println!("✓ Database opened successfully");

    if show_stats {
        // Get approximate key count
        let approx_keys = db
            .property_int_value("rocksdb.estimate-num-keys")?
            .unwrap_or(0);
        println!("  Approximate keys: {}", approx_keys);

        // Get SST file sizes
        let sst_size = db
            .property_int_value("rocksdb.total-sst-files-size")?
            .unwrap_or(0);
        println!(
            "  Total SST size: {} bytes ({:.2} GB)",
            sst_size,
            sst_size as f64 / 1_073_741_824.0
        );

        // Count actual keys by iterating (sample)
        let mut count = 0u64;
        let mut pending = 0u64;
        let mut archived = 0u64;
        let iter = db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            match item {
                Ok((_, value)) => {
                    count += 1;
                    if !value.is_empty() {
                        match value[0] {
                            1 => pending += 1,
                            2 => archived += 1,
                            _ => {}
                        }
                    } else {
                        archived += 1; // Legacy empty value = archived
                    }
                    // Print progress every 10M keys
                    if count.is_multiple_of(10_000_000) {
                        println!("  Scanned {} keys...", count);
                    }
                }
                Err(e) => {
                    println!("  ⚠ Error reading key at position {}: {}", count, e);
                }
            }
        }
        println!("  Actual key count: {}", count);
        println!("  Pending events: {}", pending);
        println!("  Archived events: {}", archived);
    }

    Ok(())
}

fn copy_database(src: &PathBuf, dest: &PathBuf) -> Result<()> {
    println!(
        "Copying valid data from {} to {}",
        src.display(),
        dest.display()
    );

    // Check destination doesn't exist
    if dest.exists() {
        anyhow::bail!("Destination path already exists: {}", dest.display());
    }

    // Open source (try to open even if potentially corrupted)
    let mut src_opts = Options::default();
    src_opts.create_if_missing(false);

    let src_db = DBWithThreadMode::<MultiThreaded>::open(&src_opts, src)
        .context("Failed to open source database")?;

    // Create destination with same settings as production
    let mut dest_opts = Options::default();
    dest_opts.create_if_missing(true);
    dest_opts.set_write_buffer_size(64 * 1024 * 1024);
    dest_opts.set_max_write_buffer_number(3);
    dest_opts.set_target_file_size_base(64 * 1024 * 1024);

    let mut block_opts = rocksdb::BlockBasedOptions::default();
    block_opts.set_bloom_filter(10.0, false);
    block_opts.set_cache_index_and_filter_blocks(true);
    dest_opts.set_block_based_table_factory(&block_opts);
    dest_opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
    dest_opts.increase_parallelism(num_cpus::get() as i32);
    dest_opts.set_max_background_jobs(4);

    let dest_db = DBWithThreadMode::<MultiThreaded>::open(&dest_opts, dest)
        .context("Failed to create destination database")?;

    // Copy all valid keys
    let mut copied = 0u64;
    let mut errors = 0u64;
    let mut batch = WriteBatch::default();
    let batch_size = 10000;

    let iter = src_db.iterator(rocksdb::IteratorMode::Start);
    for item in iter {
        match item {
            Ok((key, value)) => {
                batch.put(&key, &value);
                copied += 1;

                if copied.is_multiple_of(batch_size as u64) {
                    dest_db.write(batch)?;
                    batch = WriteBatch::default();
                }

                if copied.is_multiple_of(1_000_000) {
                    println!("  Copied {} keys...", copied);
                }
            }
            Err(e) => {
                errors += 1;
                if errors <= 10 {
                    println!("  ⚠ Error reading key: {}", e);
                }
            }
        }
    }

    // Write remaining batch
    if !batch.is_empty() {
        dest_db.write(batch)?;
    }

    // Flush destination
    dest_db.flush()?;

    println!("✓ Copy completed");
    println!("  Keys copied: {}", copied);
    println!("  Errors: {}", errors);

    Ok(())
}
