# AGENTS.md (aka CLAUDE.md)

This file provides context for AI coding agents working on the Pensieve codebase.

## Project Overview

Pensieve is an **archive-first Nostr indexer** written in Rust. It ingests events from Nostr relays, stores them in a canonical notepack archive (source of truth), and indexes them into ClickHouse for analytics. A Parquet lake (optional live shadow plus a historical campaign) is being built alongside ClickHouse, with DuckDB batch tools for research over it; see `docs/migration-plan.md`. The Postgres analytics serving store was abandoned on 2026-10-12 and its code removed; the API serves everything from ClickHouse.

**Key architecture decisions:**
- Archive-first: notepack segments are the source of truth; ClickHouse is a derived index
- RocksDB for deduplication (billions of event IDs, SSD-backed)
- Length-prefixed framing for segment files
- Notepack binary format for ~128 bytes/event savings over JSON

## Quick Reference

### Build & Development Commands

All commands use `just` (see `justfile`):

```bash
# Build
just build              # Debug build
just build-release      # Release build

# Check & Lint
just check              # Compile check only
just clippy             # Run clippy (strict: -D warnings)
just fmt                # Format code
just fmt-check          # Check formatting without modifying

# Test
just test               # Run all tests
just test-verbose       # Run tests with output

# Precommit (run before committing)
just precommit          # Runs fmt, clippy, test

# Run binaries (debug mode)
just run-ingest         # Live relay ingestion
just run-serve          # API server (pensieve-serve)
just run-preview        # Preview server (static HTML event pages)
just run-backfill-jsonl # JSONL backfill
just run-backfill-proto # Protobuf backfill
just run-relay-cleanup  # Normalize and dedupe relay database

# Preview server testing
just test-preview           # Run preview tests
just test-preview-verbose   # Run with output
just coverage-preview       # HTML coverage report (requires cargo-llvm-cov)
just coverage-preview-summary  # Coverage summary
just run-preview-lookups --help  # Compact lookup plan/backfill/status/validate/bench
```

### Agent Commit Requirement

For AI agents making code changes:

- `just precommit` is the mandatory final verification step.
- Run `just precommit` after all edits are complete and immediately before creating a commit.
- Do not create a commit if `just precommit` fails.

### ClickHouse Commands

```bash
just ch-init                  # Initialize schema from docs/clickhouse_self_hosted.sql
just ch-migrate <file>        # Run a single migration
just ch-migrate-all           # FRESH INSTALLS ONLY: replays every top-level migration (see below)
just ch-query "<sql>"         # Run raw SQL query
just ch-tables                # List tables and views
just ch-test-up               # Disposable ClickHouse 24.8 for integration tests
just test-clickhouse          # ClickHouse integration tests (needs ch-test-up)
```

> **Never run `just ch-migrate-all` against a database that already has data
> (production included).** There is no migration ledger: it re-runs every
> top-level `docs/migrations/*.sql`, and several of them drop, truncate and
> rebuild derived tables (004, 005, 006, 008, 012, 013, 014 drop and recreate
> tables; 009 runs `TRUNCATE TABLE zap_amounts_data` and a full backfill; 005
> and 008 tell you to stop services first). On production that means hours of
> full-table scans and empty or wrong analytics until the backfills finish.
> Apply individual new migrations with `just ch-migrate <file>`.

## Project Structure

```
pensieve/
├── crates/
│   ├── pensieve-core/       # Core types, validation, notepack encoding, metrics
│   ├── pensieve-ingest/     # Ingestion pipeline (relay source, dedupe, segments, ClickHouse, Parquet shadow)
│   │   └── src/
│   │       ├── bin/         # backfill-jsonl, backfill-proto, relay-cleanup, repair-dedupe,
│   │       │                #   recover-events, recover-clickhouse-residual, pensieve-negentropy-worker
│   │       ├── pipeline/    # dedupe, segment, seal_timer, admission, clickhouse, parquet_shadow, coverage
│   │       ├── relay/       # Relay quality tracking and management
│   │       ├── source/      # Event sources: relay.rs, jsonl.rs, proto.rs
│   │       └── sync/        # NIP-77 negentropy reconciliation (in-process and isolated worker)
│   ├── pensieve-preview/    # Static HTML preview pages for Nostr events
│   │   └── src/
│   │       ├── render/      # HTML renderers (note, profile, article, video, etc.)
│   │       └── routes/      # HTTP routes (preview, JSON API, llms.txt, health)
│   │   └── src/bin/         # preview-lookups (compact lookup operator tool)
│   ├── pensieve-serve/      # HTTP API (/api/v1 stats, kinds, relays) over ClickHouse
│   ├── pensieve-parquet/    # Canonical Parquet schema/writer (notepack-to-parquet, validate, compare, salvage)
│   ├── pensieve-lake/       # Parquet lake work units, inventory, publication
│   │                        #   (pensieve-parquet-campaign, pensieve-source-manifest, pensieve-lake-catalog)
│   └── pensieve-analytics/  # DuckDB analytics builders over Parquet snapshots (research tooling)
│                            #   (pensieve-analytics + pensieve-analytics-* builder/ClickHouse-compare tools)
├── docs/
│   ├── clickhouse_self_hosted.sql  # Base ClickHouse schema for fresh installs (not a production snapshot)
│   ├── migrations/                  # ClickHouse migrations (+ prepared/ and rollback/)
│   └── *.md                         # Design notes, plans and incident write-ups
├── ops/                     # Production ops: compose, systemd units + drop-in records, scripts, RUNBOOK
├── scripts/                 # Developer scripts (caching baseline, Parquet interop check)
├── data/                    # Local data directories (gitignored; ingest defaults)
│   ├── dedupe/              # RocksDB dedupe index (--rocksdb-path default)
│   ├── relays/seed.txt      # Seed relay list (--seed-file default)
│   ├── relay-stats.db       # Relay quality SQLite (--relay-db-path default)
│   └── sync-state/          # Negentropy sync state (segments default to ./segments)
└── justfile                 # Task runner commands
```

## Code Style & Conventions

### Rust Configuration
- **Edition**: 2024
- **Minimum Rust version**: 1.90
- **Formatting**: Default `cargo fmt` (no custom `.rustfmt.toml`)
- **Linting**: `cargo clippy -- -D warnings` (all warnings are errors)

### Coding Patterns

Review [rust-code-style.mdc](./.cursor/rules/rust-code-style.mdc) for full code style details.

### Documentation

- All public items should have doc comments (`///` or `//!`)
- Module-level docs at the top of each file explaining purpose
- Follow the existing pattern in `crates/pensieve-core/src/lib.rs`

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `nostr`, `nostr-sdk` | Nostr protocol types and relay connectivity |
| `notepack` | Binary encoding format for Nostr events |
| `rocksdb` | Embedded key-value store for dedupe index |
| `clickhouse` | ClickHouse client for analytics indexing |
| `tokio` | Async runtime |
| `axum` | HTTP server framework (pensieve-serve, pensieve-preview) |
| `tracing` | Structured logging |
| `metrics` + `metrics-exporter-prometheus` | Metrics export |
| `clap` | CLI argument parsing |
| `parquet`, `arrow-*` | Canonical Parquet archive writing (pensieve-parquet) |
| `aws-sdk-s3` | Object storage for the Parquet lake |
| `duckdb` | Analytics builds over the Parquet lake (pensieve-analytics) |
| `rusqlite` | Relay stats, negentropy job ledger, Parquet inventories |

## Domain Knowledge

### Nostr Concepts
- **Event**: The fundamental data unit (has `id`, `pubkey`, `kind`, `content`, `tags`, `sig`, `created_at`)
- **NIP**: Nostr Implementation Possibility (protocol spec documents)
- **NIP-01**: Basic protocol (event structure, signatures)
- **NIP-42**: Client authentication
- **NIP-65**: Relay list metadata (used for relay discovery)
- **Event kinds**: Integers identifying event types (e.g., 1 = text note, 10002 = relay list)

### Relay Connectivity
- **Seed relays**: Curated list in `data/relays/seed.txt`, protected from eviction
- **Discovery**: NIP-65 relay lists used to discover new relays automatically
- **Quality scoring**: Relays tracked in SQLite with novel rate + uptime scores
- **Tor support**: `.onion` relays supported via `--tor-proxy` flag (requires Tor daemon)

### Notepack Format
- Binary encoding by jb55 (Damus creator)
- Stores hex fields (id, pubkey, sig) as raw 32/64 bytes
- ~128 bytes smaller per event than JSON
- Streaming parser for efficient reading

### Pipeline Flow
1. **Source** (relay/JSONL/protobuf) → validated events
2. **DedupeIndex** (RocksDB) → filter duplicates by event ID
3. **SegmentWriter** → append to gzipped notepack segment
4. **ClickHouseIndexer** → index sealed segments for analytics
5. **Parquet shadow** (optional, enabled by `/etc/pensieve/parquet-shadow.env` in production) → convert sealed segments to Parquet and publish them to object storage

### Segment Lifecycle
- Events written to current segment with length-prefix framing
- Segment sealed at the size threshold (`--segment-size`, default 256 MB) or by the periodic seal timer (`--archive-seal-interval-secs`, default 300 s), whichever comes first
- Sealed segments are indexed to ClickHouse, handed to the Parquet shadow when it is active, and synced hourly to the Storage Box by `ops/scripts/sync-archive.sh`

## Testing

```bash
# Run all tests
just test

# Run with output visible
just test-verbose

# Run specific test
cargo test -p pensieve-core test_name
```

Every crate has unit tests; `pensieve-ingest`, `-preview`, `-lake`, `-parquet` and `-analytics` also have `tests/` integration suites. ClickHouse integration tests are opt-in (`just ch-test-up && just test-clickhouse`), and `just ops-test` covers the Python ops scripts.

## Common Tasks

### Adding a new CLI flag
1. Add the field to the `Args` struct in the relevant binary's `main.rs`
2. Use `#[arg(...)]` attributes from clap
3. Update the README.md if it's a user-facing flag

### Adding a new metric
1. Use `gauge!()`, `counter!()`, or `histogram!()` from the `metrics` crate
2. Follow naming convention: `<component>_<metric_name>` (e.g., `ingest_events_received_total`)
3. Register in the metrics initialization if using labels

### Modifying ClickHouse schema
1. Add a migration file in `docs/migrations/` with the next free number (`019_description.sql` at the time of writing; `prepared/` and `rollback/` share the numbering)
2. Ensure idempotency (use `IF NOT EXISTS`, `CREATE OR REPLACE`, etc.)
3. Run with `just ch-migrate docs/migrations/019_description.sql`
4. If a fresh install should get the change, update `docs/clickhouse_self_hosted.sql` too, or say in the migration why not

### Working with the dedupe index
- Keys: 32-byte event IDs (raw bytes)
- Values: 1-byte status flag (pending or archived)
- Use `check_and_mark_pending()` before writing, `mark_archived()` after sealing

---

## ClickHouse Migrations

Migrations live in `docs/migrations/` and are numbered sequentially (e.g., `001_active_user_views.sql`, `009_fix_zap_amounts.sql`).
`docs/migrations/prepared/` holds operations that need an explicit preflight (016 and 018 drop projections; 017 creates the preview compact lookups) and `docs/migrations/rollback/` holds their reversals. Neither directory is run by `just ch-migrate-all`.

**Production state (2026-10-11):** 001–015 and prepared 016, 017 and 018 are applied. `events_local` has no projections. There is no ledger table; the record is `docs/migrations/README.md` and the RUNBOOK.

### Writing Migrations

1. Create a new file: `docs/migrations/NNN_description.sql`
2. Add a header comment explaining what the migration does
3. **Ensure idempotency**: Use `IF NOT EXISTS`, `CREATE OR REPLACE`, `DROP ... IF EXISTS`
4. Include verification queries at the end to confirm the migration worked

Example migration structure:

```sql
-- Migration: NNN_description
-- Description: What this migration does
-- Date: YYYY-MM-DD
--
-- To run: just ch-migrate docs/migrations/NNN_description.sql

-- Schema changes
DROP VIEW IF EXISTS my_view;
CREATE MATERIALIZED VIEW IF NOT EXISTS my_view ...

-- Data backfill (if needed)
INSERT INTO my_table SELECT ... FROM source_table WHERE ...

-- Verification
SELECT 'Migration complete' AS status;
SELECT count() FROM my_table;
```

### Running Migrations

```bash
# Production / any existing database: apply only the new migration
just ch-migrate docs/migrations/NNN_description.sql

# Fresh, empty database only (see the warning under ClickHouse Commands)
just ch-init                  # needs the nostr database; the production compose creates it
just ch-migrate-all           # 001–015
just ch-migrate docs/migrations/prepared/017_preview_compact_lookups.sql

# Verify current state
just ch-tables
```

### Migration Considerations

- **Not replay-safe**: "idempotent" here only means "won't fail when objects already exist". Re-running 004, 005, 006, 008, 012, 013 or 014 drops and rebuilds derived tables, and re-running 009 truncates `zap_amounts_data` and backfills it from `events_local`. Never re-run an applied migration on production without reading it first.
- **Truncate + backfill**: Some migrations truncate tables and repopulate from `events_local` (2.4 billion rows in production). These take a long time.
- **Materialized views**: When fixing MV logic, you typically: drop the view, recreate with new logic, truncate the target table, and backfill.
- **Prepared operations**: Files under `docs/migrations/prepared/` are excluded from `just ch-migrate-all` and require explicit operator preflight and invocation.
- **Base schema vs production**: `docs/clickhouse_self_hosted.sql` is the fresh-install starting point; 013 later drops several objects it creates. A fresh install (base schema + 001–015 + prepared 017) produces the same tables and views as production, but `events_local` keeps the `events_by_kind` and `events_by_author` projections, which production does not have (see the header of that file).
- **Test locally first**: `just ch-test-up` starts a disposable ClickHouse 24.8 (same line as production).

---

## Production Deployment

The production host is `ssh pensieve` (user `pensieve`). The default branch is
`master`. Operational config lives in `ops/` (compose, systemd units, scripts);
secrets live outside the repo in `/etc/pensieve/` (`pensieve.env` plus
root-only `analytics.env`, `parquet-shadow.env` and others). See
[`ops/RUNBOOK.md`](ops/RUNBOOK.md) for the authoritative deploy procedure; the
summary below is a quick reference.

> **The `~/pensieve` checkout does not hold the service binaries.** The
> Pensieve services run release builds selected by systemd drop-ins (below), so
> `git pull` plus `just build-release` in `~/pensieve` deploys nothing. The
> checkout still matters: `pensieve.service` runs its `ops/production/compose.yml`,
> and `archive-sync` runs its `ops/scripts/`. It
> was brought back to a clean `master` on 2026-10-12 (after running 30 commits
> behind with hand-applied edits). Keep it clean: change those files through
> `master`, then `git -C ~/pensieve merge --ff-only origin/master`, and recreate
> containers deliberately when a mounted config or `compose.yml` changes.

### Architecture

| Component | Runs as | What actually starts (2026-10-11) |
|-----------|---------|-----------------------------------|
| ClickHouse, Grafana, Prometheus, Caddy | Docker Compose via `pensieve.service` | `~/pensieve/ops/production/compose.yml` |
| `pensieve-serve` | `pensieve-api.service` | `~/pensieve/releases/api-f60f003/pensieve-serve` (master, since 2026-10-12) via `pensieve-api.service.d/90-postgres-cutover.conf` |
| `pensieve-preview` | `pensieve-preview.service` | `~/pensieve/releases/preview-3ab1045/pensieve-preview` via `pensieve-preview.service.d/10-release.conf` |
| `pensieve-ingest` | `pensieve-ingest.service` | `/data/pensieve-build/negentropy-ba0c4a1/release/pensieve-ingest` (PR #72 build, since 2026-10-12 08:29 CEST) via `pensieve-ingest.service.d/zzzz-bounded-canary.conf` (sets `Restart=no`); `stop-timeout.conf` sets `TimeoutStopSec=15min` |
| `pensieve-negentropy-worker` | `pensieve-negentropy-worker.service` (static) | `/usr/local/libexec/pensieve-negentropy-worker` |
| Archive sync | `archive-sync.timer` | scripts in the `~/pensieve/ops/scripts/` checkout |
| Uptime Kuma push probes | `pensieve-archive-admission-kuma.timer`, `pensieve-negentropy-kuma.timer` | `/usr/local/libexec/pensieve-*-kuma.py` |

The `ExecStart` lines in `ops/systemd/*.service` still point at
`~/pensieve/target/release/`; production overrides them. Copies of the
drop-ins are in [`ops/systemd/dropins/`](ops/systemd/dropins/README.md).
Always confirm what is running before and after a change:

```bash
systemctl show pensieve-preview -p DropInPaths -p ExecStart
readlink /proc/$(systemctl show -p MainPID --value pensieve-preview)/exe
```

### Deployment Pattern

Per component, as done for `api-*` and `preview-3ab1045` (full steps in the RUNBOOK):

1. Build the exact commit in a detached worktree with its own target dir, e.g.
   `git -C ~/pensieve fetch origin && git -C ~/pensieve worktree add --detach ~/pensieve-preview-<sha> <sha>`, then
   `CARGO_TARGET_DIR=~/pensieve-preview-target cargo build --release --locked -p pensieve-preview`.
2. Copy the binaries into a new root-owned `~/pensieve/releases/<component>-<sha>/`.
3. Point the unit's drop-in at it (`ExecStart=` then the new `ExecStart=`), `sudo systemctl daemon-reload`, restart that one service.
4. Verify `/proc/<pid>/exe`, the journal and the health endpoint. Keep the previous release directory.

Migrations are applied separately and explicitly (`just ch-migrate <file>`), never with `just ch-migrate-all`.

### Service Management

```bash
# View logs
journalctl -u pensieve-ingest -f      # Ingester logs
journalctl -u pensieve-api -f         # API logs
journalctl -u pensieve-preview -f     # Preview server logs
journalctl -u 'pensieve*' -f          # All Pensieve logs

# Restart services (the ingester has a graceful-stop checklist in the RUNBOOK)
sudo systemctl restart pensieve-api
sudo systemctl restart pensieve-preview
sudo systemctl restart pensieve-ingest

# Check status
sudo systemctl status pensieve pensieve-api pensieve-ingest pensieve-preview

# Docker infrastructure
docker logs -f pensieve-clickhouse
docker logs -f pensieve-grafana
# Compose commands need the production env file, e.g.
# cd ~/pensieve/ops/production && docker compose --env-file /etc/pensieve/pensieve.env ps
```

### Environment Variables

`/etc/pensieve/pensieve.env` is read by Docker Compose (`--env-file`), by
`pensieve-api` and `pensieve-preview`, and by the `archive-sync` script
(`EnvironmentFile`). The ingester does
not read it: it takes CLI flags plus the optional root-only
`/etc/pensieve/parquet-shadow.env`, so ingest settings placed in `pensieve.env`
are silently ignored. Templates: `env.production.example`,
`ops/*.env.example`.

`pensieve-serve` (full table in [`crates/pensieve-serve/README.md`](crates/pensieve-serve/README.md)):

| Variable | Required | Description |
|----------|----------|-------------|
| `PENSIEVE_API_TOKENS` | Yes | Comma-separated API tokens |
| `PENSIEVE_BIND_ADDR` | No | Bind address (default `0.0.0.0:8080`) |
| `CLICKHOUSE_URL` | No | Default `http://localhost:8123` |
| `CLICKHOUSE_DATABASE` | No | Default `nostr` |
| `PENSIEVE_LATEST_EVENT_WATERMARK_PATH`, `PENSIEVE_LATEST_EVENT_WATERMARK_MAX_AGE_SECS` | No | Ingest-written watermark; when set, `latest_event` is read from it instead of scanning `events_local` (production: `/var/lib/pensieve/latest-event-watermark.json`; max age default 900 s) |
| `RELAY_DB_PATH` | No | Ingester's relay-stats SQLite for relay endpoints (production: `/data/relay-stats.db`, set in the API drop-in since 2026-10-12; opened once with `immutable=1`) |

`pensieve-preview` (`crates/pensieve-preview/src/config.rs`):

| Variable | Default | Description |
|----------|---------|-------------|
| `PREVIEW_BIND_ADDR` | `0.0.0.0:8081` | Bind address |
| `CLICKHOUSE_URL` | `http://localhost:8123` | ClickHouse URL |
| `CLICKHOUSE_DATABASE` | `nostr` | Database name |
| `PREVIEW_BASE_URL` | `http://localhost:8081` | Base URL for links and OG tags |
| `PREVIEW_SITE_NAME` | `Nostr Preview` | Site name |
| `PREVIEW_VIP_FILE` | `.vip` | VIP pubkeys file |
| `PREVIEW_COMPACT_LOOKUPS` | `none` | `none`, `all`, or a comma list of `profiles`, `zaps`, `addressable`; unknown values are an error. Production: `all`. Since migration 018, `none` means 10–30 s table scans |
| `PREVIEW_DISABLE_PROJECTIONS` | `false` | Run every preview query with projections off and bounded |

### Rollback Procedure

Point the service back at the previous release directory; do not rebuild from
the `~/pensieve` checkout.

```bash
# 1. Edit the drop-in to the previous release (e.g. releases/api-9829378)
sudoedit /etc/systemd/system/pensieve-api.service.d/90-postgres-cutover.conf

# 2. Reload and restart only that service
sudo systemctl daemon-reload
sudo systemctl restart pensieve-api

# 3. Confirm the binary
readlink /proc/$(systemctl show -p MainPID --value pensieve-api)/exe
```

- Removing a drop-in falls back to `~/pensieve/target/release/<binary>`, which is an older build (the preview there predates compact lookups and would do 10–30 s scans now that `events_by_kind` is gone). Only remove a drop-in after checking what that fallback binary is.
- `pensieve-preview` has only one release directory (`preview-3ab1045`); build the previous commit into `releases/` before you need it.
- For the ingester, follow the RUNBOOK's graceful-stop checks before and after.
- Schema: use the matching `docs/migrations/rollback/` file where one exists (the 018 rollback needs 1.6 TiB unreserved and is effectively unavailable). Otherwise reverse the change by hand.

### Data Directories

| Path | Purpose | Storage |
|------|---------|---------|
| `/data/clickhouse/` | ClickHouse data | NVMe (RAID1) |
| `/data/rocksdb/` | Dedupe index | NVMe |
| `/data/relay-stats.db` | Relay quality SQLite | NVMe |
| `/data/sync-state/`, `/data/negentropy/` | Negentropy sync state and isolated-reconciliation ledgers | NVMe |
| `/data/pensieve-build/` | Cargo target dirs for ingester release builds | NVMe |
| `/var/lib/pensieve/` | Ingester state dir (`latest-event-watermark.json`) | NVMe |
| `/archive/segments/` | Notepack segments (+ `.parquet-shadow/` inventory) | HDD |
| `~/pensieve/data/relays/` | Seed list and relay discovery data | NVMe |
| `~/pensieve/releases/` | Release binaries the services run | NVMe |

---

## Local Development

> **Note:** The bundled local Docker stack (`pensieve-local/`) was removed during the
> lakehouse migration. For now, run the Rust binaries natively against a ClickHouse
> you provide.

```bash
# Copy and edit local env (loaded by pensieve-serve and pensieve-preview only;
# the ingester takes CLI flags and does not read .env)
cp env.local.example .env

# Run API server (needs a reachable ClickHouse)
export PENSIEVE_API_TOKENS=dev-token
just run-serve

# Run ingester in a separate terminal (configured via CLI flags)
just run-ingest --help
```

## Security Notes

- Events are validated (ID + signature) per NIP-01 before archiving
- Invalid events are never stored
- The daemon generates ephemeral keypairs for NIP-42 relay authentication

## License

PolyForm Noncommercial License 1.0.0 - see `LICENSE` file.
