"""Credential-safe reconciliation monitor behavior; no live notifications."""

import contextlib
import importlib.util
import io
from pathlib import Path
import unittest
import urllib.error
from unittest.mock import MagicMock, patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/negentropy-kuma.py"
SPEC = importlib.util.spec_from_file_location("negentropy_kuma", SCRIPT)
KUMA = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(KUMA)


class KumaTests(unittest.TestCase):
    def test_exact_success_only(self):
        for code in (200, 204, 302, 503):
            response = MagicMock()
            response.status = code
            response.read.return_value = b""
            with patch.object(KUMA, "opener") as make:
                make.return_value.open.return_value = response
                healthy, message = KUMA.probe("http://127.0.0.1/health/negentropy")
            self.assertEqual(healthy, code == 200)
            self.assertEqual(message, f"negentropy progress HTTP {code}")

    def test_only_bounded_allowlisted_reasons_are_forwarded(self):
        for body in (b"unresolved_failures", b"secret URL", b"idle" + b" " * 100, b"\xff"):
            response = MagicMock()
            response.status = 503
            response.read.return_value = body
            with patch.object(KUMA, "opener") as make:
                make.return_value.open.return_value = response
                healthy, message = KUMA.probe("unused")
            self.assertFalse(healthy)
            response.read.assert_called_once_with(65)
            expected = "negentropy progress HTTP 503"
            if body == b"unresolved_failures":
                expected += " (unresolved_failures)"
            self.assertEqual(message, expected)

    def test_http_error_reason_is_allowlisted_too(self):
        error = urllib.error.HTTPError("unused", 503, "ignored", {}, io.BytesIO(b"capacity_exhausted"))
        with patch.object(KUMA, "opener") as make:
            make.return_value.open.side_effect = error
            self.assertEqual(KUMA.probe("unused"), (False, "negentropy progress HTTP 503 (capacity_exhausted)"))

    def test_exception_body_is_not_forwarded(self):
        with patch.object(KUMA, "opener") as make:
            make.return_value.open.side_effect = RuntimeError("secret URL")
            self.assertEqual(KUMA.probe("unused"), (False, "negentropy progress unavailable"))

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
