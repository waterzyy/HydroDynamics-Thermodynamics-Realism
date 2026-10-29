#!/usr/bin/env python3
"""Packages the compiled module into a `.hmod` — the archive the Horizon engine
and the mod browser install from.

A `.hmod` is a plain ZIP with a fixed, flat layout:

```
horizon-mod.toml        the manifest, with `[wasm].file` rewritten to the
                        artifact's name inside the archive
hydrodynamics.wasm      the cdylib for wasm32-unknown-unknown
README.md               player- and admin-facing documentation
CHANGELOG.md            what changed in this version
SHA256SUMS              checksums of everything above
```

Rewriting `[wasm].file` is the one bit of cleverness here: the working tree
points at `target/wasm32-unknown-unknown/release/hydrodynamics.wasm` (correct for
a development checkout), while an installed package must load the file sitting
next to its own manifest. Doing it at packaging time keeps both worlds honest
instead of asking the engine to guess.

Usage:
    scripts/package.py --wasm target/.../hydrodynamics.wasm --out dist/hydrodynamics-1.5.0.hmod
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WASM_NAME = "hydrodynamics.wasm"
# Fixed timestamp so that packaging the same inputs twice yields the same bytes:
# a reproducible artifact is what makes the checksum in the release notes useful.
ZIP_EPOCH = (2020, 1, 1, 0, 0, 0)


def manifest_version(manifest: str) -> str:
    match = re.search(r'^version\s*=\s*"([^"]+)"', manifest, re.MULTILINE)
    if not match:
        raise SystemExit("error: the manifest has no [mod].version")
    return match.group(1)


def rewrite_manifest(manifest: str) -> str:
    """Points `[wasm].file` at the artifact inside the archive."""
    out = []
    in_wasm_table = False
    for line in manifest.splitlines(keepends=True):
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_wasm_table = stripped == "[wasm]"
        if in_wasm_table and re.match(r"^\s*file\s*=", line):
            line = 'file             = "%s"\n' % WASM_NAME
        out.append(line)
    return "".join(out)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def add(zf: zipfile.ZipFile, name: str, data: bytes) -> None:
    info = zipfile.ZipInfo(name, date_time=ZIP_EPOCH)
    info.compress_type = zipfile.ZIP_DEFLATED
    info.external_attr = 0o644 << 16
    zf.writestr(info, data)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Build a .hmod package.")
    ap.add_argument("--wasm", required=True, help="compiled wasm32 artifact")
    ap.add_argument("--out", required=True, help="path of the .hmod to write")
    ap.add_argument("--manifest", default=str(ROOT / "horizon-mod.toml"))
    args = ap.parse_args(argv)

    wasm = Path(args.wasm)
    if not wasm.is_file():
        print(f"error: {wasm} does not exist (build it first)", file=sys.stderr)
        return 1
    manifest_path = Path(args.manifest)
    manifest = manifest_path.read_text(encoding="utf-8")
    version = manifest_version(manifest)
    packaged_manifest = rewrite_manifest(manifest).encode("utf-8")

    members: list[tuple[str, bytes]] = [
        ("horizon-mod.toml", packaged_manifest),
        (WASM_NAME, wasm.read_bytes()),
    ]
    for extra in ("README.md", "CHANGELOG.md"):
        path = ROOT / extra
        if path.is_file():
            members.append((extra, path.read_text(encoding="utf-8").encode("utf-8")))
    checksums = "".join(
        f"{hashlib.sha256(data).hexdigest()}  {name}\n" for name, data in members
    )
    members.append(("SHA256SUMS", checksums.encode("utf-8")))

    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, data in members:
            add(zf, name, data)

    print(f"packed {out} ({out.stat().st_size} bytes)")
    print(f"  version : {version}")
    print(f"  wasm    : {wasm.stat().st_size} bytes")
    print(f"  wasm sha256: {sha256(wasm)}")
    for name, data in members:
        print(f"  {name:20} {len(data):>9} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
