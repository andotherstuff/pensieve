#!/usr/bin/python3
"""Push ClickHouse index backlog health to Kuma without logging its credential."""

import json
import math
import os
from pathlib import Path
import sys
import urllib.error
import urllib.parse
import urllib.request

# Same threshold as the PensieveClickHouseIndexBacklog Prometheus rule.
MAX_PENDING_SECONDS = 900
# The ingester's /metrics page is a few hundred KiB; refuse anything far larger.
MAX_METRICS_BYTES = 8 * 1024 * 1024
OLDEST = "clickhouse_index_oldest_pending_seconds"
QUEUED = "clickhouse_index_queue_segments"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Never forward monitor credentials to a redirect destination."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def opener():
    return urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


def gauges(text, names):
    """Unlabelled gauge values from Prometheus text exposition."""
    found = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0] in names:
            try:
                value = float(parts[1])
            except ValueError:
                continue
            if math.isfinite(value):
                found[parts[0]] = value
    return found


def probe(endpoint):
    """Healthy while the oldest journaled segment is younger than the threshold."""
    try:
        with opener().open(endpoint, timeout=5) as response:
            if response.status != 200:
                return False, f"clickhouse index metrics HTTP {response.status}"
            body = response.read(MAX_METRICS_BYTES + 1)
        if len(body) > MAX_METRICS_BYTES:
            return False, "clickhouse index metrics too large"
        values = gauges(body.decode("utf-8", errors="replace"), {OLDEST, QUEUED})
    except urllib.error.HTTPError as error:
        return False, f"clickhouse index metrics HTTP {error.code}"
    except Exception:
        return False, "clickhouse index metrics unavailable"
    if OLDEST not in values:
        return False, "clickhouse index backlog metric missing"
    oldest = int(values[OLDEST])
    queued = int(values.get(QUEUED, 0))
    message = f"clickhouse index backlog {queued} segments, oldest {oldest}s"
    return oldest < MAX_PENDING_SECONDS, message


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
    healthy, message = probe(f"http://127.0.0.1:{port}/metrics")
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
        print("ClickHouse index Kuma push failed; missing-heartbeat alert remains authoritative", file=sys.stderr)
        return 2
    print(message + "; Kuma accepted")
    return 0 if healthy else 1


if __name__ == "__main__":
    raise SystemExit(main())
