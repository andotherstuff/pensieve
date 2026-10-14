"""Credential-safe ClickHouse index backlog monitor behavior; no live notifications."""

import contextlib
import importlib.util
import io
from pathlib import Path
import unittest
import urllib.error
from unittest.mock import MagicMock, patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/clickhouse-index-kuma.py"
SPEC = importlib.util.spec_from_file_location("clickhouse_index_kuma", SCRIPT)
KUMA = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(KUMA)

METRICS = b"""# HELP clickhouse_index_queue_segments Sealed segments in the index journal
# TYPE clickhouse_index_queue_segments gauge
clickhouse_index_queue_segments 2
# TYPE clickhouse_index_oldest_pending_seconds gauge
clickhouse_index_oldest_pending_seconds %s
clickhouse_index_retries_total 7
"""


def response(status, body):
    mock = MagicMock()
    mock.status = status
    mock.read.return_value = body
    mock.__enter__.return_value = mock
    return mock


def probe_with(status, body):
    with patch.object(KUMA, "opener") as make:
        make.return_value.open.return_value = response(status, body)
        return KUMA.probe("http://127.0.0.1/metrics")


class KumaTests(unittest.TestCase):
    def test_backlog_below_threshold_is_healthy(self):
        self.assertEqual(
            probe_with(200, METRICS % b"12.5"),
            (True, "clickhouse index backlog 2 segments, oldest 12s"),
        )

    def test_backlog_at_threshold_is_down(self):
        healthy, message = probe_with(200, METRICS % b"900")
        self.assertFalse(healthy)
        self.assertEqual(message, "clickhouse index backlog 2 segments, oldest 900s")

    def test_missing_metric_is_down(self):
        self.assertEqual(
            probe_with(200, b"clickhouse_index_queue_segments 0\n"),
            (False, "clickhouse index backlog metric missing"),
        )

    def test_labelled_or_invalid_samples_are_ignored(self):
        body = b'clickhouse_index_oldest_pending_seconds{x="y"} 5\nclickhouse_index_oldest_pending_seconds NaN\n'
        self.assertEqual(probe_with(200, body), (False, "clickhouse index backlog metric missing"))

    def test_non_200_and_oversized_are_down(self):
        self.assertEqual(probe_with(503, b""), (False, "clickhouse index metrics HTTP 503"))
        big = b"x" * (KUMA.MAX_METRICS_BYTES + 1)
        self.assertEqual(probe_with(200, big), (False, "clickhouse index metrics too large"))

    def test_http_error_and_exception_bodies_are_not_forwarded(self):
        error = urllib.error.HTTPError("unused", 502, "secret", {}, io.BytesIO(b"secret"))
        with patch.object(KUMA, "opener") as make:
            make.return_value.open.side_effect = error
            self.assertEqual(KUMA.probe("unused"), (False, "clickhouse index metrics HTTP 502"))
        with patch.object(KUMA, "opener") as make:
            make.return_value.open.side_effect = RuntimeError("secret URL")
            self.assertEqual(KUMA.probe("unused"), (False, "clickhouse index metrics unavailable"))

    def test_redirects_are_refused(self):
        self.assertIsNone(KUMA.NoRedirect().redirect_request(None, None, 302, "", {}, "https://elsewhere"))

    def test_destinations_are_pinned(self):
        valid = "https://status.ipf.dev/api/push/test-token"
        self.assertEqual(KUMA.destination(valid), valid)
        for bad in (
            "http://status.ipf.dev/api/push/test-token",
            "https://evil.example/api/push/test-token",
            "https://status.ipf.dev@evil.example/api/push/test-token",
            valid + "?extra=1",
            valid + "#fragment",
            "https://status.ipf.dev/api/push/",
        ):
            with self.assertRaises(ValueError):
                KUMA.destination(bad)

    def test_missing_credential_fails_without_exception_details(self):
        output = io.StringIO()
        with patch.object(KUMA.sys, "argv", ["probe"]), patch.dict(KUMA.os.environ, {}, clear=True), \
             patch.object(KUMA, "probe", return_value=(True, "healthy")), contextlib.redirect_stderr(output):
            self.assertEqual(KUMA.main(), 2)
        self.assertIn("missing-heartbeat", output.getvalue())
        self.assertNotIn("Traceback", output.getvalue())


if __name__ == "__main__":
    unittest.main()
