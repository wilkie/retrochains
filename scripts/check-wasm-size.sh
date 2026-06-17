#!/usr/bin/env bash
# Size-budget guard for the @retrochains/bcc WASM module — the analog of the
# fingerprint Jetpack coverage *floor*, but a *ceiling*: fail if the built module
# grows past budget (e.g. the decompiler accidentally pulling a second compiler
# in, or a dependency bloating). Run after scripts/build-wasm.sh, in CI.
#
# Ceilings are on the raw and gzipped .wasm. They sit comfortably above the
# current size (BCC forward toolchain: ~1.1 MB raw / ~0.32 MB gz, pre-wasm-opt)
# with headroom; tighten them as wasm-opt/brotli land.
set -euo pipefail

cd "$(dirname "$0")/.."
WASM="packages/bcc/wasm/bcc_wasm_bg.wasm"
MAX_RAW_KB="${MAX_RAW_KB:-1500}"
MAX_GZ_KB="${MAX_GZ_KB:-450}"

[ -f "$WASM" ] || { echo "error: $WASM not found — run scripts/build-wasm.sh first" >&2; exit 1; }

raw_kb=$(( $(stat -c%s "$WASM") / 1024 ))
gz_kb=$(( $(gzip -9 -c "$WASM" | wc -c) / 1024 ))
echo "wasm size: ${raw_kb} KB raw (budget ${MAX_RAW_KB}), ${gz_kb} KB gzip (budget ${MAX_GZ_KB})"

fail=0
[ "$raw_kb" -gt "$MAX_RAW_KB" ] && { echo "FAIL: raw size ${raw_kb} KB exceeds ${MAX_RAW_KB} KB" >&2; fail=1; }
[ "$gz_kb" -gt "$MAX_GZ_KB" ] && { echo "FAIL: gzip size ${gz_kb} KB exceeds ${MAX_GZ_KB} KB" >&2; fail=1; }
exit "$fail"
