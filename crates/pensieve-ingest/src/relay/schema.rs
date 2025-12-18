//! SQLite schema for relay statistics tracking.
//!
//! This module defines the database schema and provides migration utilities
//! for the relay stats database.

use rusqlite::{Connection, Result};

/// Current schema version. Increment when making breaking changes.
pub const SCHEMA_VERSION: i32 = 1;

/// Initialize the database schema.
///
/// Creates all tables if they don't exist and runs any pending migrations.
pub fn init_schema(conn: &Connection) -> Result<()> {
    // Check current version
    let current_version = get_schema_version(conn)?;

    if current_version == 0 {
        // Fresh database - create all tables
        create_tables(conn)?;
        set_schema_version(conn, SCHEMA_VERSION)?;
    } else if current_version < SCHEMA_VERSION {
        // Run migrations
        migrate(conn, current_version, SCHEMA_VERSION)?;
    }

    Ok(())
}

/// Get the current schema version (0 if not initialized).
fn get_schema_version(conn: &Connection) -> Result<i32> {
    // Create version table if it doesn't exist
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER NOT NULL
        )",
        [],
    )?;

    let version: Option<i32> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .ok();

    Ok(version.unwrap_or(0))
}

/// Set the schema version.
fn set_schema_version(conn: &Connection, version: i32) -> Result<()> {
    conn.execute("DELETE FROM schema_version", [])?;
    conn.execute("INSERT INTO schema_version (version) VALUES (?)", [version])?;
    Ok(())
}

/// Create all tables for a fresh database.
fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- Core relay registry
        CREATE TABLE IF NOT EXISTS relays (
            url TEXT PRIMARY KEY,
            first_seen_at INTEGER NOT NULL,
            last_connected_at INTEGER,
            tier TEXT NOT NULL DEFAULT 'discovered',
            status TEXT NOT NULL DEFAULT 'pending',
            consecutive_failures INTEGER NOT NULL DEFAULT 0,
            blocked_reason TEXT,
            notes TEXT
        );

        -- Hourly stats buckets
        CREATE TABLE IF NOT EXISTS relay_stats_hourly (
            relay_url TEXT NOT NULL,
            hour_start INTEGER NOT NULL,
            events_received INTEGER NOT NULL DEFAULT 0,
            events_novel INTEGER NOT NULL DEFAULT 0,
            events_duplicate INTEGER NOT NULL DEFAULT 0,
            connection_attempts INTEGER NOT NULL DEFAULT 0,
            connection_successes INTEGER NOT NULL DEFAULT 0,
            connection_seconds INTEGER NOT NULL DEFAULT 0,
            disconnects INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (relay_url, hour_start)
        );

        -- Daily rollups (kept forever)
        CREATE TABLE IF NOT EXISTS relay_stats_daily (
            relay_url TEXT NOT NULL,
            day_start INTEGER NOT NULL,
            events_received INTEGER NOT NULL DEFAULT 0,
            events_novel INTEGER NOT NULL DEFAULT 0,
            connection_attempts INTEGER NOT NULL DEFAULT 0,
            connection_successes INTEGER NOT NULL DEFAULT 0,
            connection_seconds INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (relay_url, day_start)
        );

        -- Materialized scores
        CREATE TABLE IF NOT EXISTS relay_scores (
            relay_url TEXT PRIMARY KEY,
            score REAL NOT NULL,
            novel_rate_1h REAL,
            novel_rate_24h REAL,
            novel_rate_7d REAL,
            uptime_24h REAL,
            uptime_7d REAL,
            last_computed_at INTEGER NOT NULL
        );

        -- Indexes for efficient queries
        CREATE INDEX IF NOT EXISTS idx_relays_status ON relays(status);
        CREATE INDEX IF NOT EXISTS idx_relays_tier ON relays(tier);
        CREATE INDEX IF NOT EXISTS idx_relay_stats_hourly_time ON relay_stats_hourly(hour_start);
        CREATE INDEX IF NOT EXISTS idx_relay_stats_daily_time ON relay_stats_daily(day_start);
        CREATE INDEX IF NOT EXISTS idx_relay_scores_score ON relay_scores(score DESC);
        "#,
    )?;

    Ok(())
}

/// Run migrations from one version to another.
fn migrate(conn: &Connection, from: i32, to: i32) -> Result<()> {
    for version in from..to {
        match version {
            // Add migration cases here as schema evolves
            // 1 => migrate_v1_to_v2(conn)?,
            _ => {
                // No migration needed for this version
            }
        }
    }
    set_schema_version(conn, to)?;
    Ok(())
}

/// Relay tier classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayTier {
    /// Manually curated, always-connect relays.
    Seed,
    /// Discovered via NIP-65 or georelays import.
    Discovered,
}

impl RelayTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Discovered => "discovered",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "seed" => Some(Self::Seed),
            "discovered" => Some(Self::Discovered),
            _ => None,
        }
    }
}

/// Relay connection status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayStatus {
    /// Never connected yet.
    Pending,
    /// Currently connected and receiving events.
    Active,
    /// Known relay, not currently connected.
    Idle,
    /// Recent connection failures.
    Failing,
    /// Blocked after too many failures.
    Blocked,
}

impl RelayStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Failing => "failing",
            Self::Blocked => "blocked",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "active" => Some(Self::Active),
            "idle" => Some(Self::Idle),
            "failing" => Some(Self::Failing),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_init_schema_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Verify tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"relays".to_string()));
        assert!(tables.contains(&"relay_stats_hourly".to_string()));
        assert!(tables.contains(&"relay_stats_daily".to_string()));
        assert!(tables.contains(&"relay_scores".to_string()));
    }

    #[test]
    fn test_init_schema_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap(); // Should not fail
    }

    #[test]
    fn test_relay_tier_roundtrip() {
        assert_eq!(RelayTier::from_str(RelayTier::Seed.as_str()), Some(RelayTier::Seed));
        assert_eq!(
            RelayTier::from_str(RelayTier::Discovered.as_str()),
            Some(RelayTier::Discovered)
        );
    }

    #[test]
    fn test_relay_status_roundtrip() {
        for status in [
            RelayStatus::Pending,
            RelayStatus::Active,
            RelayStatus::Idle,
            RelayStatus::Failing,
            RelayStatus::Blocked,
        ] {
            assert_eq!(RelayStatus::from_str(status.as_str()), Some(status));
        }
    }
}

