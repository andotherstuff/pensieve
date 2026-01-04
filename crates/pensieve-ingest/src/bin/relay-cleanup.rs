//! Relay database cleanup utility.
//!
//! This tool normalizes relay URLs and removes duplicates and invalid entries
//! from the relay SQLite database.
//!
//! # What It Does
//!
//! 1. Normalizes URLs (removes trailing slashes, etc.)
//! 2. Removes obviously invalid URLs (localhost, private IPs, .onion, etc.)
//! 3. Merges duplicate entries (keeping the one with best stats)
//! 4. Shows a summary of changes
//!
//! # Usage
//!
//! ```bash
//! # Dry run (show what would be done, don't modify)
//! relay-cleanup --db ./data/relay-stats.db --dry-run
//!
//! # Actually perform cleanup
//! relay-cleanup --db ./data/relay-stats.db
//! ```

use anyhow::{Context, Result};
use clap::Parser;
use pensieve_ingest::relay::url::{NormalizeResult, normalize_relay_url};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::PathBuf;

/// Relay database cleanup utility.
#[derive(Parser, Debug)]
#[command(name = "relay-cleanup")]
#[command(about = "Normalize relay URLs and remove duplicates from the relay database")]
#[command(version)]
struct Args {
    /// Path to the relay SQLite database
    #[arg(long, short, default_value = "./data/relay-stats.db")]
    db: PathBuf,

    /// Dry run - show what would be done without making changes
    #[arg(long)]
    dry_run: bool,

    /// Verbose output - show each URL being processed
    #[arg(long, short)]
    verbose: bool,
}

/// Statistics about a relay URL (used to pick best duplicate).
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // Some fields used only for diagnostics/future expansion
struct RelayStats {
    url: String,
    tier: String,
    status: String,
    first_seen_at: i64,
    last_connected_at: Option<i64>,
    consecutive_failures: i64,
    events_received: i64,
    events_novel: i64,
    score: Option<f64>,
}

impl RelayStats {
    /// Score for picking the "best" entry when merging duplicates.
    /// Higher is better.
    fn merge_priority(&self) -> i64 {
        let tier_score = match self.tier.as_str() {
            "seed" => 1000,
            _ => 0,
        };
        let status_score = match self.status.as_str() {
            "active" => 100,
            "idle" => 50,
            "pending" => 25,
            "failing" => 10,
            "blocked" => 0,
            _ => 0,
        };
        // Prefer entries with more events and better uptime
        tier_score + status_score + self.events_novel
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    println!("Relay Database Cleanup");
    println!("======================");
    println!("Database: {}", args.db.display());
    println!("Dry run: {}", args.dry_run);
    println!();

    if !args.db.exists() {
        anyhow::bail!("Database file not found: {}", args.db.display());
    }

    // Open database
    let mut conn = Connection::open(&args.db)
        .with_context(|| format!("Failed to open database: {}", args.db.display()))?;

    // Enable WAL mode for safety
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

    // Stats
    let mut normalized = 0usize;
    let mut merged_duplicates = 0usize;

    // Step 1: Load all relays with their stats
    println!("Loading relays...");
    let relays = load_all_relays(&conn)?;
    let total_relays = relays.len();
    println!("Found {} relays", total_relays);

    // Step 2: Normalize and categorize
    let mut normalized_map: HashMap<String, Vec<RelayStats>> = HashMap::new();
    let mut invalid_urls: Vec<String> = Vec::new();
    let mut blocked_urls: Vec<String> = Vec::new();

    for relay in &relays {
        match normalize_relay_url(&relay.url) {
            NormalizeResult::Ok(norm) => {
                if norm != relay.url {
                    normalized += 1;
                    if args.verbose {
                        println!("  Normalize: {} -> {}", relay.url, norm);
                    }
                }
                normalized_map.entry(norm).or_default().push(relay.clone());
            }
            NormalizeResult::Invalid(reason) => {
                invalid_urls.push(relay.url.clone());
                if args.verbose {
                    println!("  Invalid: {} ({})", relay.url, reason);
                }
            }
            NormalizeResult::Blocked(reason) => {
                blocked_urls.push(relay.url.clone());
                if args.verbose {
                    println!("  Blocked: {} ({})", relay.url, reason);
                }
            }
        }
    }

    let removed_invalid = invalid_urls.len();
    let removed_blocked = blocked_urls.len();

    // Count duplicates
    for entries in normalized_map.values() {
        if entries.len() > 1 {
            merged_duplicates += entries.len() - 1;
        }
    }

    // Summary
    println!();
    println!("Summary");
    println!("-------");
    println!("Total relays:         {}", total_relays);
    println!("URLs normalized:      {}", normalized);
    println!("Invalid URLs:         {}", removed_invalid);
    println!("Blocked URLs:         {}", removed_blocked);
    println!("Duplicate entries:    {}", merged_duplicates);
    println!("Final relay count:    {}", normalized_map.len());
    println!();

    if args.dry_run {
        println!("Dry run - no changes made.");
        println!();

        // Show some examples
        if !invalid_urls.is_empty() {
            println!("Sample invalid URLs (first 10):");
            for url in invalid_urls.iter().take(10) {
                println!("  - {}", url);
            }
            println!();
        }

        if !blocked_urls.is_empty() {
            println!("Sample blocked URLs (first 10):");
            for url in blocked_urls.iter().take(10) {
                println!("  - {}", url);
            }
            println!();
        }

        // Show duplicates
        let duplicates: Vec<_> = normalized_map
            .iter()
            .filter(|(_, entries)| entries.len() > 1)
            .take(10)
            .collect();
        if !duplicates.is_empty() {
            println!("Sample duplicate groups (first 10):");
            for (norm, entries) in duplicates {
                println!("  {} ({} entries):", norm, entries.len());
                for entry in entries {
                    println!(
                        "    - {} (tier={}, events={})",
                        entry.url, entry.tier, entry.events_novel
                    );
                }
            }
            println!();
        }

        return Ok(());
    }

    // Step 3: Apply changes
    println!("Applying changes...");

    let tx = conn.transaction()?;

    // Delete invalid and blocked URLs
    for url in &invalid_urls {
        delete_relay(&tx, url)?;
    }
    for url in &blocked_urls {
        delete_relay(&tx, url)?;
    }

    // Process normalized map - merge duplicates
    for (normalized_url, entries) in &normalized_map {
        if entries.len() == 1 {
            // Single entry - just update URL if it changed
            let entry = &entries[0];
            if entry.url != *normalized_url {
                update_relay_url(&tx, &entry.url, normalized_url)?;
            }
        } else {
            // Multiple entries - keep the best one, delete the rest
            let mut sorted = entries.clone();
            sorted.sort_by_key(|e| std::cmp::Reverse(e.merge_priority()));

            let best = &sorted[0];
            let rest = &sorted[1..];

            // Merge stats from all entries into the best one
            let earliest_first_seen = entries.iter().map(|e| e.first_seen_at).min().unwrap_or(0);
            let latest_connected = entries.iter().filter_map(|e| e.last_connected_at).max();

            // Delete the duplicate entries
            for entry in rest {
                delete_relay(&tx, &entry.url)?;
            }

            // Update the best entry with normalized URL and merged stats
            tx.execute(
                "UPDATE relays SET url = ?, first_seen_at = ?, last_connected_at = ? WHERE url = ?",
                params![
                    normalized_url,
                    earliest_first_seen,
                    latest_connected,
                    best.url
                ],
            )?;

            // Update hourly stats to point to normalized URL
            tx.execute(
                "UPDATE relay_stats_hourly SET relay_url = ? WHERE relay_url = ?",
                params![normalized_url, best.url],
            )?;

            // Merge hourly stats from duplicates
            for entry in rest {
                // Sum up hourly stats into the canonical URL
                tx.execute(
                    "UPDATE relay_stats_hourly
                     SET events_received = events_received + COALESCE(
                         (SELECT SUM(events_received) FROM relay_stats_hourly WHERE relay_url = ?), 0
                     ),
                     events_novel = events_novel + COALESCE(
                         (SELECT SUM(events_novel) FROM relay_stats_hourly WHERE relay_url = ?), 0
                     )
                     WHERE relay_url = ?",
                    params![entry.url, entry.url, normalized_url],
                )?;

                // Delete old hourly stats
                tx.execute(
                    "DELETE FROM relay_stats_hourly WHERE relay_url = ?",
                    params![entry.url],
                )?;
            }

            // Update scores table
            tx.execute(
                "UPDATE relay_scores SET relay_url = ? WHERE relay_url = ?",
                params![normalized_url, best.url],
            )?;
            for entry in rest {
                tx.execute(
                    "DELETE FROM relay_scores WHERE relay_url = ?",
                    params![entry.url],
                )?;
            }
        }
    }

    tx.commit()?;

    println!("Done!");
    println!();

    // Verify final state
    let final_count: i64 = conn.query_row("SELECT COUNT(*) FROM relays", [], |row| row.get(0))?;
    println!("Final relay count in database: {}", final_count);

    Ok(())
}

/// Load all relays with their aggregated stats.
fn load_all_relays(conn: &Connection) -> Result<Vec<RelayStats>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            r.url,
            r.tier,
            r.status,
            r.first_seen_at,
            r.last_connected_at,
            r.consecutive_failures,
            COALESCE(SUM(h.events_received), 0) as events_received,
            COALESCE(SUM(h.events_novel), 0) as events_novel,
            s.score
        FROM relays r
        LEFT JOIN relay_stats_hourly h ON r.url = h.relay_url
        LEFT JOIN relay_scores s ON r.url = s.relay_url
        GROUP BY r.url
        ORDER BY r.url
        "#,
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(RelayStats {
            url: row.get(0)?,
            tier: row.get(1)?,
            status: row.get(2)?,
            first_seen_at: row.get(3)?,
            last_connected_at: row.get(4)?,
            consecutive_failures: row.get(5)?,
            events_received: row.get(6)?,
            events_novel: row.get(7)?,
            score: row.get(8)?,
        })
    })?;

    let mut relays = Vec::new();
    for row in rows {
        relays.push(row?);
    }

    Ok(relays)
}

/// Delete a relay and all its associated data.
fn delete_relay(conn: &Connection, url: &str) -> Result<()> {
    conn.execute("DELETE FROM relay_scores WHERE relay_url = ?", params![url])?;
    conn.execute(
        "DELETE FROM relay_stats_hourly WHERE relay_url = ?",
        params![url],
    )?;
    conn.execute(
        "DELETE FROM relay_stats_daily WHERE relay_url = ?",
        params![url],
    )?;
    conn.execute("DELETE FROM relays WHERE url = ?", params![url])?;
    Ok(())
}

/// Update a relay's URL to the normalized form.
fn update_relay_url(conn: &Connection, old_url: &str, new_url: &str) -> Result<()> {
    // Update main relays table
    conn.execute(
        "UPDATE relays SET url = ? WHERE url = ?",
        params![new_url, old_url],
    )?;
    // Update hourly stats
    conn.execute(
        "UPDATE relay_stats_hourly SET relay_url = ? WHERE relay_url = ?",
        params![new_url, old_url],
    )?;
    // Update daily stats
    conn.execute(
        "UPDATE relay_stats_daily SET relay_url = ? WHERE relay_url = ?",
        params![new_url, old_url],
    )?;
    // Update scores
    conn.execute(
        "UPDATE relay_scores SET relay_url = ? WHERE relay_url = ?",
        params![new_url, old_url],
    )?;
    Ok(())
}
