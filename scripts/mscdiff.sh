#!/bin/bash
# Usage: mscdiff.sh <fixture-name>
# Builds our msc OBJ for the fixture, disassembles both ours and expected
# _TEXT, and shows a side-by-side / unified diff of the disassembly.
set -e
F="$1"
ROOT=/home/wilkie/retrochains
FX="$ROOT/fixtures/$F"
TMP=$(mktemp -d)
cp "$FX/HELLO.C" "$TMP/HELLO.C"
( cd "$TMP" && "$ROOT/target/debug/msc" /c /Fa /AS HELLO.C >/dev/null 2>&1 || true )
python3 "$ROOT/scripts/objdis.py" "$TMP/HELLO.OBJ" > "$TMP/ours.asm" 2>/dev/null || echo "(ours failed)" > "$TMP/ours.asm"
python3 "$ROOT/scripts/objdis.py" "$FX/expected/msc/HELLO.OBJ" > "$TMP/gold.asm"
echo "===== $F ====="
echo "--- C source ---"; cat "$FX/HELLO.C"
echo "--- diff (gold=left/<  ours=right/>) ---"
diff -y -W 100 "$TMP/gold.asm" "$TMP/ours.asm" || true
rm -rf "$TMP"
