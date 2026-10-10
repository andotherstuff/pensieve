#!/usr/bin/env python3
"""Read-only, time-bounded ledger measurements; never authorizes pruning or rollout."""

import argparse
import json
from pathlib import Path
import sqlite3
import time


def inspect(path, timeout=5.0):
    """Return aggregate counts only, without lease tokens or event identifiers."""
    path = Path(path).resolve(strict=True)
    deadline = time.monotonic() + timeout
    db = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=timeout)
    try:
        db.execute("PRAGMA query_only=ON")
        db.set_progress_handler(lambda: int(time.monotonic() >= deadline), 1000)
        db.execute("BEGIN")
        identity = (
            db.execute("PRAGMA application_id").fetchone()[0],
            db.execute("PRAGMA user_version").fetchone()[0],
        )
        if identity != (0x504E4A31, 8):
            raise ValueError("unsupported ledger identity/version")
        states = dict(db.execute("SELECT state,count(*) FROM jobs GROUP BY state"))
        rows = sum(states.values())
        if rows > 100_000:
            raise ValueError("ledger exceeds default supported job bound")
        attempts = db.execute(
            "SELECT count(*),coalesce(sum(received),0),"
            "coalesce(sum(archived),0),coalesce(sum(excluded),0) FROM attempts"
        ).fetchone()
        leased_attempts = db.execute(
            "SELECT coalesce(sum(attempt),0) FROM jobs"
        ).fetchone()[0]
        receipts = db.execute("SELECT count(*) FROM receipts").fetchone()[0]
        exclusions = db.execute("SELECT count(*) FROM policy_exclusions").fetchone()[0]
        if receipts != db.execute(
            "SELECT retained FROM receipt_totals WHERE singleton=1"
        ).fetchone()[0]:
            raise ValueError("receipt counter mismatch")
        if exclusions != db.execute(
            "SELECT retained FROM exclusion_totals WHERE singleton=1"
        ).fetchone()[0]:
            raise ValueError("exclusion counter mismatch")
        pinned = db.execute(
            "SELECT count(DISTINCT first_job) FROM policy_exclusions"
        ).fetchone()[0]
        oldest = db.execute(
            "SELECT min(since) FROM jobs WHERE state!='complete'"
        ).fetchone()[0]
        failures = db.execute("SELECT count(*) FROM failure_reports").fetchone()[0]
        # No identifiers or lease capabilities are emitted. Event time is not a
        # creation/completion timestamp and cannot measure queue latency.
        logical_bytes = db.execute("PRAGMA page_count").fetchone()[0] * db.execute(
            "PRAGMA page_size"
        ).fetchone()[0]
        freelist = db.execute("PRAGMA freelist_count").fetchone()[0]
        db.rollback()
    finally:
        db.close()
    # Filesystem samples are outside the SQL snapshot and may change concurrently.
    sizes = {}
    for suffix in ("", "-wal", "-shm"):
        try:
            sizes[suffix or "database"] = Path(str(path) + suffix).stat().st_size
        except FileNotFoundError:
            if not suffix:
                raise
            sizes[suffix] = 0
    return {
        "schema": 1,
        "ledger_schema": identity[1],
        "observed_at_unix": int(time.time()),
        "states": states,
        "retained_jobs": rows,
        "default_planner_slots_remaining": max(0, 100_000 - 2048 - rows),
        "attempt_summaries": attempts[0],
        "leased_attempts": leased_attempts,
        "received_frames": attempts[1],
        "archived_frames": attempts[2],
        "excluded_frames": attempts[3],
        "unresolved_receipts": receipts,
        "policy_exclusions": exclusions,
        "jobs_pinned_by_exclusion_proof": pinned,
        "failure_reports": failures,
        "oldest_unresolved_event_since": oldest,
        "logical_database_bytes": logical_bytes,
        "freelist_pages": freelist,
        "file_bytes_nonatomic": sizes,
        "default_admission_budget_bytes": 1024**3,
        "completion_freshness": "unknown: schema 8 has no completion timestamp",
        "safe_to_prune": False,
        "continuous_ready": False,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger", type=Path)
    args = parser.parse_args()
    try:
        result = inspect(args.ledger)
    except (OSError, sqlite3.Error, ValueError, TypeError) as error:
        # Do not echo arbitrary paths, SQL, or database text in error output.
        print(json.dumps({"status": "unavailable", "error_type": type(error).__name__}))
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
