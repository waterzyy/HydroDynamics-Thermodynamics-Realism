#!/usr/bin/env python3
"""Verify that the WASM artifact exports every symbol `horizon-mod.toml` promises.

The manifest is the contract the engine loads the mod against: if it lists an
entry point that the compiled `cdylib` does not actually export, the host fails
at *load* time — in production, on the player's machine, with a message that
points nowhere. This script turns that into a CI failure with the exact symbol
name.

Usage:
    scripts/check-exports.py target/wasm32-unknown-unknown/release/hydrodynamics.wasm \
                              horizon-mod.toml

It parses the export section of the module directly (no wasm tooling needed, so
it runs in any CI image) and compares it with `[wasm.exports]`. `memory` and the
toolchain's own symbols (`__heap_base`, `__data_end`, ...) are ignored: they are
not part of the mod ABI.
"""

from __future__ import annotations

import sys

try:  # Python 3.11+
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - pragma: no cover
    import tomli as tomllib  # type: ignore[no-redef]


class WasmError(Exception):
    """The file is not a well-formed WebAssembly module."""


def read_uleb(data: bytes, pos: int) -> tuple[int, int]:
    """Decodes an unsigned LEB128 integer, returning `(value, new_pos)`."""
    result = 0
    shift = 0
    while True:
        if pos >= len(data):
            raise WasmError("truncated LEB128 integer")
        byte = data[pos]
        pos += 1
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return result, pos
        shift += 7
        if shift > 63:
            raise WasmError("LEB128 integer is too long")


def exports(path: str) -> set[str]:
    """Returns the names in the module's export section."""
    with open(path, "rb") as fh:
        data = fh.read()
    if len(data) < 8 or data[:4] != b"\x00asm":
        raise WasmError(f"{path}: not a WebAssembly module")
    if int.from_bytes(data[4:8], "little") != 1:
        raise WasmError(f"{path}: unsupported WebAssembly version")

    out: set[str] = set()
    pos = 8
    while pos < len(data):
        section_id = data[pos]
        pos += 1
        size, pos = read_uleb(data, pos)
        end = pos + size
        if end > len(data):
            raise WasmError(f"{path}: section {section_id} runs past the end")
        if section_id == 7:  # export section
            count, p = read_uleb(data, pos)
            for _ in range(count):
                name_len, p = read_uleb(data, p)
                name = data[p : p + name_len].decode("utf-8")
                p += name_len
                p += 1  # external kind
                _index, p = read_uleb(data, p)
                out.add(name)
        pos = end
    return out


def declared(manifest: str) -> set[str]:
    with open(manifest, "rb") as fh:
        cfg = tomllib.load(fh)
    table = cfg.get("wasm", {}).get("exports", {})
    return {v for v in table.values() if isinstance(v, str)}


IGNORED = {"memory", "__heap_base", "__data_end", "__indirect_function_table"}


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__)
        return 2
    wasm_path, manifest_path = argv[1], argv[2]
    try:
        have = exports(wasm_path)
    except (OSError, WasmError) as exc:
        print(f"error: {exc}")
        return 1
    want = declared(manifest_path)

    missing = sorted(want - have)
    if missing:
        print("error: the manifest declares symbols the artifact does not export:")
        for name in missing:
            print(f"  - {name}")
        return 1

    extra = sorted(n for n in have - want if n not in IGNORED and not n.startswith("__"))
    if extra:
        # Not fatal: an extra export costs nothing and may be a debug helper.
        print(f"note: {len(extra)} export(s) not listed in the manifest: {', '.join(extra)}")

    print(f"ok: all {len(want)} declared exports are present ({len(have)} exports in total)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
