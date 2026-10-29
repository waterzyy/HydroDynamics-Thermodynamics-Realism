#!/usr/bin/env python3
"""Builds the GitHub release notes from CHANGELOG.md plus the artifact checksums.

Keeping the notes generated has one purpose: the release body can never describe
a different version than the archive attached to it. The version comes from the
tag, the prose comes from the changelog section for that version, and the
checksum is computed from the `.hmod` that is about to be uploaded.

Usage:
    scripts/release-notes.py --version 1.5.0 --hmod dist/hydrodynamics-1.5.0.hmod \
                             --out dist/release-notes.md
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def section(changelog: str, version: str) -> str:
    """Returns the `## [version]` body of the changelog."""
    pattern = re.compile(r"^## \[%s\][^\n]*\n(.*?)(?=^## \[|\Z)" % re.escape(version),
                         re.MULTILINE | re.DOTALL)
    match = pattern.search(changelog)
    if not match:
        raise SystemExit(f"error: no '## [{version}]' section in CHANGELOG.md")
    return match.group(1).strip()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Generate the release notes.")
    ap.add_argument("--version", required=True)
    ap.add_argument("--hmod", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--changelog", default=str(ROOT / "CHANGELOG.md"))
    args = ap.parse_args(argv)

    hmod = Path(args.hmod)
    if not hmod.is_file():
        raise SystemExit(f"error: {hmod} does not exist")
    changelog = Path(args.changelog).read_text(encoding="utf-8")

    body = [
        f"**{args.version}** — Horizon `horizon-mod/1.4` · `wasm32-unknown-unknown`",
        "",
        section(changelog, args.version),
        "",
        "---",
        "",
        "### Install",
        "",
        "Drop the `.hmod` into the world's `mods/` directory (or install it from the",
        "in-game mod browser) and restart the server. The archive is a flat ZIP:",
        "",
        "```",
        f"{hmod.name}",
        "├── horizon-mod.toml    manifest (wasm path already points at the packaged artifact)",
        "├── hydrodynamics.wasm  the module, zero host imports",
        "├── README.md           physics, performance and ABI documentation",
        "├── CHANGELOG.md        this list",
        "└── SHA256SUMS          checksums of everything above",
        "```",
        "",
        "### Checksums",
        "",
        "```",
        f"{sha256(hmod)}  {hmod.name}",
        "```",
        "",
        "### Verification",
        "",
        "The attached archive was produced by CI from the tagged commit after the full",
        "test suite passed, the export list was checked against the manifest and the",
        "artifact passed the 2 MiB size gate.",
        "",
    ]
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text("\n".join(body), encoding="utf-8")
    print(f"wrote {out} ({out.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
