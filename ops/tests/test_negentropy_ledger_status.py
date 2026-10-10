"""Read-only reporting regressions; no live services or credentials required."""

import importlib.util
from contextlib import closing, contextmanager
from pathlib import Path
import sqlite3
import tempfile
import unittest

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
                INSERT INTO jobs VALUES(1,'complete',10,2),(2,'retry_wait',20,1);
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
        self.assertEqual(report["retained_jobs"], 2)
        self.assertEqual(report["jobs_pinned_by_exclusion_proof"], 1)
        self.assertEqual(report["oldest_unresolved_event_since"], 20)
        self.assertEqual(report["archived_frames"], 2)
        self.assertEqual(report["leased_attempts"], 3)
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
            db.execute("PRAGMA user_version=9")
        before = self.path.read_bytes()
        with self.assertRaises(ValueError):
            STATUS.inspect(self.path)
        self.assertEqual(before, self.path.read_bytes())

    def test_receipt_counter_mismatch_is_not_healthy(self):
        with database(self.path) as db:
            db.execute("UPDATE receipt_totals SET retained=1")
        with self.assertRaises(ValueError):
            STATUS.inspect(self.path)

    def test_empty_ledger_reports_unknown_completion_not_success(self):
        with database(self.path) as db:
            db.execute("DELETE FROM jobs")
        report = STATUS.inspect(self.path)
        self.assertIsNone(report["oldest_unresolved_event_since"])
        self.assertTrue(report["completion_freshness"].startswith("unknown"))


if __name__ == "__main__":
    unittest.main()
