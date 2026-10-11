#!/usr/bin/python3
"""Report exact localhost archive admission status; never log the push secret."""
import json
import os
import pathlib
import sys
import urllib.error
import urllib.parse
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def probe(url):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        with opener.open(url, timeout=5) as response:
            code = response.status
    except urllib.error.HTTPError as error:
        code = error.code
    except Exception:
        return False, "archive admission unavailable"
    return code == 200, f"archive admission HTTP {code}"


def main():
    fixture = len(sys.argv) == 2 and sys.argv[1] == "--fixture"
    if len(sys.argv) > 1 and not fixture:
        raise SystemExit("Only --fixture is supported")
    endpoint = "http://127.0.0.1:19091/health/archive-admission" if fixture else "http://127.0.0.1:9091/health/archive-admission"
    healthy, message = probe(endpoint)
    if fixture:
        message = "COMMISSIONING TEST - " + message
    credential_dir = os.environ.get("CREDENTIALS_DIRECTORY")
    credential = pathlib.Path(credential_dir) / "push-url" if credential_dir else pathlib.Path("/etc/pensieve/archive-admission-kuma.url")
    secret = credential.read_text().strip()
    parsed = urllib.parse.urlsplit(secret)
    if parsed.scheme != "https" or parsed.netloc != "status.ipf.dev" or not parsed.path.startswith("/api/push/") or parsed.query or parsed.fragment:
        raise SystemExit("Invalid monitor destination")
    url = secret + "?" + urllib.parse.urlencode({"status": "up" if healthy else "down", "msg": message})
    try:
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        with opener.open(url, timeout=10) as response:
            accepted = response.status == 200 and json.loads(response.read(4096)).get("ok") is True
        if not accepted:
            raise ValueError("push rejected")
    except Exception:
        print("Kuma push failed; missing-heartbeat alert remains authoritative", file=sys.stderr)
        return 2
    print(message + "; Kuma accepted")
    return 0 if healthy else 1


if __name__ == "__main__":
    sys.exit(main())
