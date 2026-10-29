#!/usr/bin/env bash
# Builds the mod for the Horizon engine and reports the artifact size.
#
#   ./scripts/build-wasm.sh [--opt] [--release|--debug]
#
# --opt additionally runs `wasm-opt -Oz` when it is installed (the module has no
# imports, so it links standalone and optimises very well).
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET=wasm32-unknown-unknown
PROFILE=release
RUN_OPT=0

for arg in "$@"; do
  case "$arg" in
    --opt)     RUN_OPT=1 ;;
    --debug)   PROFILE=debug ;;
    --release) PROFILE=release ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "installing $TARGET ..."
  rustup target add "$TARGET"
fi

if [ "$PROFILE" = "release" ]; then
  cargo build --release --target "$TARGET"
else
  cargo build --target "$TARGET"
fi

OUT="target/$TARGET/$PROFILE/hydrodynamics.wasm"
test -f "$OUT" || { echo "expected artifact missing: $OUT" >&2; exit 1; }

if [ "$RUN_OPT" = "1" ]; then
  if command -v wasm-opt >/dev/null 2>&1; then
    wasm-opt -Oz --strip-debug --enable-bulk-memory "$OUT" -o "${OUT%.wasm}.min.wasm"
    echo "optimised: ${OUT%.wasm}.min.wasm"
  else
    echo "wasm-opt not found — skipping the size pass (binaryen package)" >&2
  fi
fi

SIZE=$(wc -c < "$OUT")
printf 'artifact: %s (%s bytes)\n' "$OUT" "$SIZE"

# A healthy build is far below the engine's 2 MiB per-mod soft cap.
if [ "$SIZE" -gt 2097152 ]; then
  echo "warning: artifact exceeds the 2 MiB engine soft cap" >&2
fi

# The module must not import anything from the host.
if command -v wasm-objdump >/dev/null 2>&1; then
  if wasm-objdump -x "$OUT" | grep -q 'Import\['; then
    echo "warning: the module has host imports; check Cargo.toml profiles" >&2
  else
    echo "imports: none (fully self-contained module)"
  fi
fi
