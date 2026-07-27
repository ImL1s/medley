#!/usr/bin/env python3
"""Decode the split audit manifest, then invoke the core issue publisher."""

from __future__ import annotations

import base64
import gzip
import subprocess
import sys
import tempfile
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
PARTS_GLOB = ".github/audit/2026-07-28-issues.json.gz.b64.part-*"
CORE_PUBLISHER = REPO_ROOT / "scripts/publish-audit-issues.py"


def die(message: str) -> "NoReturn":
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(2)


def main() -> int:
    parts = sorted(REPO_ROOT.glob(PARTS_GLOB))
    if not parts:
        die(f"no manifest parts matched {PARTS_GLOB}")

    actual = [part.name.rsplit(".part-", 1)[1] for part in parts]
    expected = [f"{index:02d}" for index in range(1, len(parts) + 1)]
    if actual != expected:
        die(f"manifest parts must be contiguous from 01; found {actual}")

    try:
        encoded = b"".join(part.read_bytes() for part in parts)
        decoded = gzip.decompress(base64.b64decode(encoded, validate=True))
    except (OSError, ValueError, gzip.BadGzipFile) as exc:
        die(f"invalid split manifest: {exc}")

    if not CORE_PUBLISHER.is_file():
        die(f"core publisher not found: {CORE_PUBLISHER}")

    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as handle:
        manifest_path = Path(handle.name)
        handle.write(decoded)

    try:
        command = [
            sys.executable,
            str(CORE_PUBLISHER),
            "--manifest",
            str(manifest_path),
            *sys.argv[1:],
        ]
        return subprocess.run(command, check=False).returncode
    finally:
        manifest_path.unlink(missing_ok=True)


if __name__ == "__main__":
    raise SystemExit(main())
