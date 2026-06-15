#!/bin/bash
# Usage: mscdiff.sh <fixture-name>
# Builds our msc OBJ for the fixture, disassembles both ours and expected
# _TEXT, and shows a side-by-side / unified diff of the disassembly.
set -e
F="$1"
ROOT=/home/wilkie/retrochains
# Resolve the fixture under the language-organized tree: accept a full
# sub-path (`c/bitfields/foo`) or a bare name (`foo`/`123-foo`, tried under c/).
if [ -d "$ROOT/fixtures/$F" ]; then FX="$ROOT/fixtures/$F";
elif [ -d "$ROOT/fixtures/c/$F" ]; then FX="$ROOT/fixtures/c/$F";
else FX=$(dirname "$(find "$ROOT/fixtures" -maxdepth 6 -type d -name "$F" -print -quit)"/.); fi
TMP=$(mktemp -d)
cp "$FX/HELLO.C" "$TMP/HELLO.C"
( cd "$TMP" && "$ROOT/target/debug/msc" /c /Fa /AS HELLO.C >/dev/null 2>&1 || true )
python3 "$ROOT/scripts/objdis.py" "$TMP/HELLO.OBJ" > "$TMP/ours.asm" 2>/dev/null || echo "(ours failed)" > "$TMP/ours.asm"
# The golden OBJ is a gitignored, reproducible cache (only its sha256 lives in
# expected/msc/manifest.toml). On a fresh checkout it isn't present, so re-drive
# the oracle to materialize it — `materialize` also asserts the reproduction
# matches the recorded hash before writing.
GOLD="$FX/expected/msc/HELLO.OBJ"
if [ ! -f "$GOLD" ]; then
  "$ROOT/target/debug/xfix" materialize --compiler msc "$FX" >/dev/null 2>&1 \
    || { echo "(could not materialize golden OBJ for $F — is the MSC oracle installed?)"; exit 1; }
fi
python3 "$ROOT/scripts/objdis.py" "$GOLD" > "$TMP/gold.asm"
echo "===== $F ====="
echo "--- C source ---"; cat "$FX/HELLO.C"
echo "--- diff (gold=left/<  ours=right/>) ---"
diff -y -W 100 "$TMP/gold.asm" "$TMP/ours.asm" || true
rm -rf "$TMP"
