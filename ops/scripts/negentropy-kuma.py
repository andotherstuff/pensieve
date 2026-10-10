#!/usr/bin/python3
"""Push private reconciliation health to Kuma without logging its credential."""

import json
import os
from pathlib import Path
import sys
import urllib.error
import urllib.parse
import urllib.request

REASONS = frozenset({
    "disabled", "starting", "idle", "progress_within_deadline",
    "state_unavailable", "runtime_failed", "owner_observation_stale",
    "capacity_exhausted", "unresolved_failures", "durable_progress_stalled",
})


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Never forward monitor credentials to a redirect destination."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def opener():
    return urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


def probe(endpoint):
    """Only HTTP 200 is healthy; forward only bounded, fixed reason tokens."""
    try:
        try:
            response = opener().open(endpoint, timeout=5)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            code = response.status
            body = response.read(65)
        reason = body.decode("ascii", errors="replace") if len(body) <= 64 else ""
        message = f"negentropy progress HTTP {code}"
        if reason in REASONS:
            message += f" ({reason})"
        return code == 200, message
    except Exception:
        return False, "negentropy progress unavailable"


def destination(secret):
    """Constrain credential destination to the commissioned Kuma instance."""
    parsed = urllib.parse.urlsplit(secret)
    if (
        parsed.scheme != "https"
        or parsed.netloc != "status.ipf.dev"
        or not parsed.path.startswith("/api/push/")
        or not parsed.path.removeprefix("/api/push/")
        or parsed.query
        or parsed.fragment
    ):
        raise ValueError("invalid monitor destination")
    return secret


def main():
    fixture = sys.argv[1:] == ["--fixture"]
    if sys.argv[1:] and not fixture:
        print("Only --fixture is supported", file=sys.stderr)
        return 2
    port = 19091 if fixture else 9091
    healthy, message = probe(f"http://127.0.0.1:{port}/health/negentropy")
    if fixture:
        message = "COMMISSIONING TEST - " + message
    try:
        # No fallback to an environment token or command-line secret.
        credential = Path(os.environ["CREDENTIALS_DIRECTORY"]) / "push-url"
        url = destination(credential.read_text().strip())
        url += "?" + urllib.parse.urlencode(
            {"status": "up" if healthy else "down", "msg": message}
        )
        with opener().open(url, timeout=10) as response:
            accepted = response.status == 200 and json.loads(response.read(4096)).get("ok") is True
        if not accepted:
            raise ValueError("push rejected")
    except Exception:
        # Exceptions may contain the credential URL; never print their values.
        print("Negentropy Kuma push failed; missing-heartbeat alert remains authoritative", file=sys.stderr)
        return 2
    print(message + "; Kuma accepted")
    return 0 if healthy else 1


if __name__ == "__main__":
    raise SystemExit(main())
