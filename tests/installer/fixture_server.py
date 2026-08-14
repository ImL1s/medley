#!/usr/bin/env python3
"""A stand-in for GitHub, so installer behaviour is testable without it.

The scenarios that matter most — an empty release channel, a missing
repository, a tampered archive, a final 3xx carrying a tag-shaped body — are
unreachable against the real API without publishing junk releases. This serves
them from a fixture instead.

Usage:  fixture_server.py <port> <scenario> <archive-dir>

Scenarios:
  ok             a normal published release (covers /releases/latest resolution)
  no-release     404 from /releases/latest, repository itself exists
  no-repo        404 from both
  redirect-body  a final 300 whose body contains a tag (issue #83)
  bad-checksum   a release whose checksums file does not match the archive
"""

import http.server
import json
import os
import sys
import urllib.parse

PORT = int(sys.argv[1])
SCENARIO = sys.argv[2]
ARCHIVE_DIR = sys.argv[3]

TAG = "v9.9.9+providers.1"
VERSION = TAG[1:]


def _read(name):
    with open(os.path.join(ARCHIVE_DIR, name), "rb") as fh:
        return fh.read()


class Handler(http.server.BaseHTTPRequestHandler):
    def _send(self, code, body, ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):  # noqa: N802 - http.server's interface
        path = self.path

        if path.endswith("/releases/latest"):
            if SCENARIO in ("no-release", "no-repo"):
                self._send(404, b'{"message":"Not Found"}')
                return
            if SCENARIO == "redirect-body":
                # A *final* 3xx: no Location, so --location has nothing to
                # follow and curl --fail does not treat it as an error.
                self._send(300, json.dumps({"tag_name": "v0.0.1+providers.1"}).encode())
                return
            self._send(200, json.dumps({"tag_name": TAG}).encode())
            return

        # The repository-existence probe the installer makes to tell "nothing
        # published" apart from "cannot reach GitHub".
        if path.rstrip("/").endswith(tuple(f"/repos/{o}" for o in ("medley-test/medley",))):
            if SCENARIO == "no-repo":
                self._send(404, b'{"message":"Not Found"}')
            else:
                self._send(200, b'{"full_name":"medley-test/medley"}')
            return

        # Release assets. The tag carries a '+', which the installer sends
        # percent-encoded and http.server does *not* decode — so decode it
        # here, or every asset request 404s on a name that differs from the
        # file on disk by exactly `%2B` versus `+`.
        name = urllib.parse.unquote(path.rsplit("/", 1)[-1])
        try:
            body = _read(name)
        except OSError:
            self._send(404, b"not found", "text/plain")
            return
        self._send(200, body, "application/octet-stream")

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    http.server.HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
