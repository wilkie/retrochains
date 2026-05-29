#!/usr/bin/env bash
# Bulk-capture MSC oracle output for BCC -obj fixtures.
#
# Workflow per fixture:
#   1. Skip if it already has an invocation.msc.toml.
#   2. Skip BCC-extension fixtures (`_AX`, `_seg`, `__asm`, etc.).
#   3. Write a `invocation.msc.toml` invoking `cl /c /AS HELLO.C`.
#   4. Run `xfix capture --compiler msc`.
#   5. Inspect manifest — if MSC actually produced HELLO.OBJ, keep the
#      invocation file; otherwise remove it and clean up expected/msc/.
#
# Runs N captures in parallel via xargs. Logs per-fixture outcome
# to scripts/bulk_msc_capture.log.

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

LOG="scripts/bulk_msc_capture.log"
JOBS="${JOBS:-8}"

# Patterns that indicate BCC-only source.
SKIP_PATTERNS='_AX|_BX|_CX|_DX|_SI|_DI|_BP|_SP|_CS|_DS|_ES|_SS|_FLAGS|_seg|__asm|__interrupt|asm\\b'

capture_one() {
  local fixture="$1"
  local name
  name="$(basename "$fixture")"

  if [[ -f "$fixture/invocation.msc.toml" ]]; then
    echo "SKIP $name (already has msc invocation)"
    return 0
  fi
  if ! [[ -f "$fixture/HELLO.C" ]]; then
    echo "SKIP $name (no HELLO.C)"
    return 0
  fi

  if grep -qE "$SKIP_PATTERNS" "$fixture/HELLO.C"; then
    echo "SKIP $name (BCC-only source)"
    return 0
  fi

  # Write a minimal MSC invocation.
  cat > "$fixture/invocation.msc.toml" <<EOF
description = "Auto-derived from $name (BCC fixture). cl /c /AS via the microsoft-msc5 oracle profile."
tool = "cl"
args = ["/c", "/AS", "HELLO.C"]
inputs = ["HELLO.C"]
EOF

  # Run capture. Use timeout to bound stuck runs.
  if ! timeout 90 target/debug/xfix capture --compiler msc "$fixture" >/dev/null 2>&1; then
    echo "FAIL-CAPTURE $name"
    rm -f "$fixture/invocation.msc.toml"
    rm -rf "$fixture/expected/msc"
    return 0
  fi

  # Inspect the captured manifest. If MSC didn't produce HELLO.OBJ,
  # the fixture is useless for our verify path.
  local manifest="$fixture/expected/msc/manifest.toml"
  if ! [[ -f "$manifest" ]]; then
    echo "NO-MANIFEST $name"
    rm -f "$fixture/invocation.msc.toml"
    rm -rf "$fixture/expected/msc"
    return 0
  fi

  if ! grep -q 'name = "HELLO.OBJ"' "$manifest"; then
    echo "MSC-REJECT $name"
    rm -f "$fixture/invocation.msc.toml"
    rm -rf "$fixture/expected/msc"
    return 0
  fi

  echo "OK $name"
}

export -f capture_one
export SKIP_PATTERNS

# Targets: every -obj fixture not already in the msc namespace.
mapfile -t TARGETS < <(
  ls -d fixtures/*-obj 2>/dev/null \
    | grep -v -- '/msc' \
    | grep -v -- '-msc-' \
    | sort
)

echo "[bulk-msc-capture] $(date)  targets=${#TARGETS[@]}  jobs=$JOBS" | tee -a "$LOG"

printf '%s\n' "${TARGETS[@]}" \
  | xargs -P "$JOBS" -I{} bash -c 'capture_one "$@"' _ {} \
  | tee -a "$LOG"

echo "[bulk-msc-capture] $(date)  done" | tee -a "$LOG"
