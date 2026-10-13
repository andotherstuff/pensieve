"""Read-only reporting regressions; no live services or credentials required."""

import importlib.util
from contextlib import closing, contextmanager
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/negentropy-ledger-status.py"
SPEC = importlib.util.spec_from_file_location("ledger_status", SCRIPT)
STATUS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(STATUS)


@contextmanager
def database(path):
    with closing(sqlite3.connect(path)) as db, db:
        yield db


class LedgerStatusTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "ledger.sqlite"
        with database(self.path) as db:
            db.executescript("""
                PRAGMA application_id=1347308081;
                PRAGMA user_version=8;
                CREATE TABLE jobs(id INTEGER PRIMARY KEY,state TEXT,since INTEGER,attempt INTEGER);
                INSERT INTO jobs VALUES(1,'complete',10,2),(2,'retry_wait',20,1),(3,'leased',30,1);
                CREATE TABLE attempts(received INTEGER,archived INTEGER,excluded INTEGER);
                INSERT INTO attempts VALUES(3,2,1);
                CREATE TABLE receipts(event_id BLOB);
                CREATE TABLE policy_exclusions(first_job INTEGER);
                INSERT INTO policy_exclusions VALUES(1);
                CREATE TABLE receipt_totals(singleton INTEGER,retained INTEGER);
                INSERT INTO receipt_totals VALUES(1,0);
                CREATE TABLE exclusion_totals(singleton INTEGER,retained INTEGER);
                INSERT INTO exclusion_totals VALUES(1,1);
                CREATE TABLE failure_reports(report TEXT);
                INSERT INTO failure_reports VALUES('secret diagnostic not for output');
            """)

    def test_counts_preserve_file_and_do_not_infer_novelty_or_freshness(self):
        before = self.path.read_bytes()
        report = STATUS.inspect(self.path)
        self.assertEqual(self.path.read_bytes(), before)
        self.assertEqual(report["retained_jobs"], 3)
        self.assertEqual(report["jobs_pinned_by_exclusion_proof"], 1)
        self.assertEqual(report["oldest_incomplete_event_since"], 20)
        self.assertEqual(report["archived_frames"], 2)
        self.assertEqual(report["lease_grants"], 4)
        self.assertEqual(report["states"]["leased"], 1)
        self.assertEqual(report["attempt_summaries"], 1)
        self.assertFalse(report["safe_to_prune"])
        self.assertFalse(report["continuous_ready"])
        self.assertNotIn("secret diagnostic", str(report))

    def test_missing_database_is_not_created(self):
        missing = self.path.parent / "missing.sqlite"
        with self.assertRaises(FileNotFoundError):
            STATUS.inspect(missing)
        self.assertFalse(missing.exists())

    def test_unknown_schema_is_not_migrated(self):
        with database(self.path) as db:
            db.execute("PRAGMA user_version=11")
        before = self.path.read_bytes()
        with self.assertRaisesRegex(STATUS.Refusal, "unsupported_identity"):
            STATUS.inspect(self.path)
        self.assertEqual(before, self.path.read_bytes())

    def test_schema9_separates_retained_detail_from_coverage_accounting(self):
        with database(self.path) as db:
            db.executescript("""
                PRAGMA user_version=9;
                CREATE TABLE lifecycle(singleton INTEGER,last_success INTEGER);
                INSERT INTO lifecycle VALUES(1,1234);
                CREATE TABLE completed_coverage(received INTEGER,archived INTEGER,excluded INTEGER,lease_grants INTEGER,failures INTEGER);
                INSERT INTO completed_coverage VALUES(10,9,1,2,1);
            """)
        report = STATUS.inspect(self.path)
        self.assertEqual(report["last_durable_completion_unix"], 1234)
        self.assertEqual(report["received_frames"], 3)
        self.assertEqual(report["retained_coverage_accounting"]["received_frames"], 10)
        self.assertFalse(report["continuous_ready"])

    def test_schema10_reports_relay_unavailable_exclusions_apart_from_success(self):
        with database(self.path) as db:
            db.executescript("""
                PRAGMA user_version=10;
                ALTER TABLE jobs ADD COLUMN reason TEXT;
                INSERT INTO jobs VALUES(4,'complete',40,15,'relay_unavailable');
                CREATE TABLE lifecycle(singleton INTEGER,last_success INTEGER);
                INSERT INTO lifecycle VALUES(1,1234);
                CREATE TABLE completed_coverage(received INTEGER,archived INTEGER,excluded INTEGER,
                    lease_grants INTEGER,failures INTEGER,relay_unavailable INTEGER);
                INSERT INTO completed_coverage VALUES(10,9,1,2,1,2);
                CREATE TABLE relay_unavailable_exclusions(id INTEGER PRIMARY KEY,event_id BLOB,
                    job INTEGER,relay TEXT,since INTEGER,until INTEGER,first_attempt INTEGER,
                    last_attempt INTEGER,attempts INTEGER,first_reported_at INTEGER,
                    last_reported_at INTEGER,span_secs INTEGER,excluded_at INTEGER,resolved_at INTEGER);
            """)
            for n in range(7):
                db.execute(
                    "INSERT INTO relay_unavailable_exclusions VALUES(?,?,?,?,0,899,1,15,15,NULL,?,32580,?,?)",
                    (n + 1, bytes([0x08, n]) + bytes(30), 4 if n == 6 else 100 + n,
                     "wss://premium.primal.net", 1000 + n, 2000 + n, 3000 if n == 0 else None),
                )
        report = STATUS.inspect(self.path)
        exclusions = report["relay_unavailable_exclusions"]
        self.assertEqual((exclusions["events"], exclusions["jobs"]), (7, 7))
        self.assertEqual(exclusions["unresolved_events"], 6)
        self.assertEqual(report["states"]["complete"], 2)
        self.assertEqual(
            exclusions["complete_states"], {"reconciled": 1, "relay_unavailable_exclusion": 1}
        )
        recent = exclusions["recent"]
        self.assertEqual(len(recent), 5)
        self.assertEqual(recent[0]["event_id"], "0806" + "00" * 30)
        self.assertEqual(recent[0]["job"], 4)
        self.assertEqual([r["excluded_at"] for r in recent], [2006, 2005, 2004, 2003, 2002])
        self.assertEqual(recent[0]["relay"], "wss://premium.primal.net")
        self.assertEqual(report["retained_coverage_accounting"]["relay_unavailable_events"], 2)
        self.assertEqual(report["retained_coverage_accounting"]["excluded_frames"], 1)
        self.assertNotIn("secret diagnostic", str(report))
        self.assertFalse(report["continuous_ready"])

    def test_schema9_has_no_relay_unavailable_section(self):
        with database(self.path) as db:
            db.executescript("""
                PRAGMA user_version=9;
                CREATE TABLE lifecycle(singleton INTEGER,last_success INTEGER);
                INSERT INTO lifecycle VALUES(1,NULL);
                CREATE TABLE completed_coverage(received INTEGER,archived INTEGER,excluded INTEGER,lease_grants INTEGER,failures INTEGER);
            """)
        report = STATUS.inspect(self.path)
        self.assertIsNone(report["relay_unavailable_exclusions"])
        self.assertNotIn("relay_unavailable_events", report["retained_coverage_accounting"])

    def test_receipt_counter_mismatch_is_not_healthy(self):
        with database(self.path) as db:
            db.execute("UPDATE receipt_totals SET retained=1")
        with self.assertRaisesRegex(STATUS.Refusal, "receipt_counter_mismatch"):
            STATUS.inspect(self.path)

    def test_empty_ledger_reports_unknown_completion_not_success(self):
        with database(self.path) as db:
            db.execute("DELETE FROM jobs")
        report = STATUS.inspect(self.path)
        self.assertIsNone(report["oldest_incomplete_event_since"])
        self.assertTrue(report["completion_freshness"].startswith("unknown"))

    def test_exclusion_counter_error_is_distinct(self):
        with database(self.path) as db:
            db.execute("UPDATE exclusion_totals SET retained=2")
        with self.assertRaisesRegex(STATUS.Refusal, "exclusion_counter_mismatch"):
            STATUS.inspect(self.path)

    def test_wrong_owner_is_refused_before_sqlite_open(self):
        with patch.object(STATUS.os, "geteuid", return_value=self.path.stat().st_uid + 1):
            with patch.object(STATUS.sqlite3, "connect") as connect:
                with self.assertRaisesRegex(STATUS.Refusal, "wrong_ledger_owner"):
                    STATUS.inspect(self.path)
                connect.assert_not_called()

    def test_closed_wal_does_not_create_sidecars(self):
        with database(self.path) as db:
            db.execute("PRAGMA journal_mode=WAL")
        before = {p.name: p.read_bytes() for p in self.path.parent.iterdir()}
        with self.assertRaisesRegex(STATUS.Refusal, "wal_sidecars_missing"):
            STATUS.inspect(self.path)
        self.assertEqual(before, {p.name: p.read_bytes() for p in self.path.parent.iterdir()})

    def test_live_wal_reads_committed_uncheckpointed_state(self):
        with database(self.path) as db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("UPDATE jobs SET attempt=3 WHERE id=1")
            db.commit()
            before = self.path.read_bytes()
            report = STATUS.inspect(self.path)
            self.assertEqual(report["lease_grants"], 5)
            self.assertEqual(before, self.path.read_bytes())
            self.assertGreater(report["file_bytes_nonatomic"]["-wal"], 0)


if __name__ == "__main__":
    unittest.main()
