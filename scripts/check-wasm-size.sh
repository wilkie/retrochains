#!/usr/bin/env bash
# Size-budget guard for a @retrochains WASM module — the analog of the fingerprint
# Jetpack coverage *floor*, but a *ceiling*: fail if the built module grows past
# budget (e.g. the decompiler accidentally pulling a compiler in, or a dependency
# bloating). Run after scripts/build-wasm.sh, in CI.
#
#   scripts/check-wasm-size.sh [PACKAGE] [OUTNAME]   (default: bcc bcc_wasm)
#
# Per-module ceilings (raw KB / gzip KB); override with MAX_RAW_KB / MAX_GZ_KB.
set -euo pipefail

cd "$(dirname "$0")/.."
PACKAGE="${1:-bcc}"
OUTNAME="${2:-bcc_wasm}"
WASM="packages/$PACKAGE/wasm/${OUTNAME}_bg.wasm"

# Defaults sit above current sizes (pre-wasm-opt) with headroom: the BCC toolchain
# module is ~1.0 MB / ~0.30 MB gz; the compiler-free decompiler is tiny.
case "$PACKAGE" in
  bcc)        DEF_RAW=1500; DEF_GZ=450 ;;
  msc)        DEF_RAW=1000; DEF_GZ=300 ;;
  decompile)  DEF_RAW=400;  DEF_GZ=120 ;;
  *)          DEF_RAW=1500; DEF_GZ=450 ;;
esac
MAX_RAW_KB="${MAX_RAW_KB:-$DEF_RAW}"
MAX_GZ_KB="${MAX_GZ_KB:-$DEF_GZ}"

[ -f "$WASM" ] || { echo "error: $WASM not found — run scripts/build-wasm.sh first" >&2; exit 1; }

raw_kb=$(( $(stat -c%s "$WASM") / 1024 ))
gz_kb=$(( $(gzip -9 -c "$WASM" | wc -c) / 1024 ))
echo "[$PACKAGE] wasm size: ${raw_kb} KB raw (budget ${MAX_RAW_KB}), ${gz_kb} KB gzip (budget ${MAX_GZ_KB})"

fail=0
[ "$raw_kb" -gt "$MAX_RAW_KB" ] && { echo "FAIL: raw size ${raw_kb} KB exceeds ${MAX_RAW_KB} KB" >&2; fail=1; }
[ "$gz_kb" -gt "$MAX_GZ_KB" ] && { echo "FAIL: gzip size ${gz_kb} KB exceeds ${MAX_GZ_KB} KB" >&2; fail=1; }
exit "$fail"
