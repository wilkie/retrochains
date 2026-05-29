#!/usr/bin/env bash
# Bulk re-capture every -obj fixture with ASM output alongside the OBJ.
#
# BCC fixtures: add `asm_args = ["-S", "-m<model>", "HELLO.C"]` to the
#   existing invocation. The harness chains both compile commands in
#   one DOSBox session so each capture stays one oracle launch.
# MSC fixtures: insert `/Fa` into the existing args list so `cl /c /Fa
#   /A<model> HELLO.C` produces HELLO.OBJ and HELLO.ASM in one run.
#
# Idempotent: skips fixtures that already carry asm_args (BCC) or
# already include /Fa (MSC). Runs N captures in parallel via xargs.

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

LOG="${LOG:-scripts/bulk_asm_recapture.log}"
JOBS="${JOBS:-8}"
COMPILER="${COMPILER:-both}"  # bcc | msc | both

# ---------------------------------------------------------------------------
# Per-fixture worker. Modifies invocation.<compiler>.toml in place, then
# re-runs capture. Logs OK / SKIP / FAIL.
# ---------------------------------------------------------------------------

ensure_bcc_asm() {
  local fixture="$1"
  local toml="$fixture/invocation.bcc.toml"
  if grep -q '^asm_args' "$toml"; then
    return 1
  fi
  # Mirror the existing args but swap -c for -S. The args field is a
  # TOML array literal on a single line; we synthesize an asm_args
  # line by string-replacing "-c" → "-S" and appending it after
  # the args line.
  local asm_line
  asm_line=$(grep '^args = ' "$toml" | sed 's/"-c"/"-S"/')
  if [[ -z "$asm_line" ]]; then
    return 2
  fi
  printf '%s\n' "${asm_line/args = /asm_args = }" >> "$toml"
  return 0
}

ensure_msc_asm() {
  local fixture="$1"
  local toml="$fixture/invocation.msc.toml"
  if grep -q '"/Fa"' "$toml"; then
    return 1
  fi
  # Insert "/Fa" right after "/c" in the args array.
  python3 - "$toml" <<'PY'
import sys, re
path = sys.argv[1]
text = open(path).read()
# Match args = ["...", "/c", ...] and inject "/Fa" after "/c".
def repl(m):
    parts = m.group(1)
    return 'args = [' + parts.replace('"/c"', '"/c", "/Fa"', 1) + ']'
new = re.sub(r'args = \[([^\]]*)\]', repl, text, count=1)
if new == text:
    sys.exit(1)
open(path, 'w').write(new)
PY
}

recapture_bcc() {
  local fixture="$1"
  local name
  name="$(basename "$fixture")"
  local toml="$fixture/invocation.bcc.toml"
  if [[ ! -f "$toml" ]]; then
    echo "SKIP $name (no bcc toml)"
    return 0
  fi
  case "$(ensure_bcc_asm "$fixture"; echo $?)" in
    1) echo "SKIP $name (already has asm_args)"; return 0;;
    2) echo "FAIL $name (no -c in args)"; return 0;;
    0) ;;
  esac
  if ! timeout 120 target/debug/xfix capture "$fixture" >/dev/null 2>&1; then
    echo "FAIL-CAPTURE-BCC $name"
    return 0
  fi
  if ! grep -q 'name = "HELLO.ASM"' "$fixture/expected/bcc/manifest.toml" 2>/dev/null; then
    echo "NO-ASM-BCC $name"
    return 0
  fi
  echo "OK-BCC $name"
}

recapture_msc() {
  local fixture="$1"
  local name
  name="$(basename "$fixture")"
  local toml="$fixture/invocation.msc.toml"
  if [[ ! -f "$toml" ]]; then
    return 0
  fi
  case "$(ensure_msc_asm "$fixture"; echo $?)" in
    1) echo "SKIP-MSC $name (already has /Fa)"; return 0;;
  esac
  if ! timeout 120 target/debug/xfix capture --compiler msc "$fixture" >/dev/null 2>&1; then
    echo "FAIL-CAPTURE-MSC $name"
    return 0
  fi
  if ! grep -q 'name = "HELLO.ASM"' "$fixture/expected/msc/manifest.toml" 2>/dev/null; then
    echo "NO-ASM-MSC $name"
    return 0
  fi
  echo "OK-MSC $name"
}

worker() {
  local fixture="$1"
  if [[ "$COMPILER" == "bcc" || "$COMPILER" == "both" ]]; then
    if [[ -f "$fixture/invocation.bcc.toml" ]]; then
      recapture_bcc "$fixture"
    fi
  fi
  if [[ "$COMPILER" == "msc" || "$COMPILER" == "both" ]]; then
    if [[ -f "$fixture/invocation.msc.toml" ]]; then
      recapture_msc "$fixture"
    fi
  fi
}

export -f worker recapture_bcc recapture_msc ensure_bcc_asm ensure_msc_asm
export COMPILER

main() {
  mapfile -t TARGETS < <(ls -d fixtures/*-obj 2>/dev/null | sort)
  echo "[bulk-asm-recapture] $(date)  targets=${#TARGETS[@]}  jobs=$JOBS  compiler=$COMPILER" | tee -a "$LOG"
  printf '%s\n' "${TARGETS[@]}" \
    | xargs -P "$JOBS" -I{} bash -c 'worker "$@"' _ {} \
    | tee -a "$LOG"
  echo "[bulk-asm-recapture] $(date)  done" | tee -a "$LOG"
}

# Run the sweep only when invoked as a script, not when sourced for
# `worker` access. Mirrors Python's `if __name__ == '__main__'`.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main
fi
