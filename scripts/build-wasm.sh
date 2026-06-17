#!/usr/bin/env bash
# Build a @retrochains WASM module from its *-wasm crate.
#
#   scripts/build-wasm.sh [CRATE] [PACKAGE] [OUTNAME]
#
# Defaults build the BCC toolchain module:
#   CRATE=bcc-wasm  PACKAGE=bcc  OUTNAME=bcc_wasm
# The others:
#   scripts/build-wasm.sh msc-wasm        msc        msc_wasm
#   scripts/build-wasm.sh decompile-wasm  decompile  decompile_wasm
#
# Pipeline:  cargo (wasm32, size-opt) -> wasm-bindgen -> wasm-opt -> packages/<PACKAGE>/wasm/
# The output is a reproducible build artifact, gitignored like the OBJ goldens.
#
# Requires the wasm32 target (pinned in rust-toolchain.toml) and wasm-bindgen-cli
# at the SAME version as the wasm-bindgen crate. wasm-opt (binaryen) is optional.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
CRATE="${1:-bcc-wasm}"
PACKAGE="${2:-bcc}"
OUTNAME="${3:-bcc_wasm}"
OUT="$ROOT/packages/$PACKAGE/wasm"
PROFILE_DIR="$ROOT/target/wasm32-unknown-unknown/release"

# The wasm-bindgen *crate* version the cdylib was built against — the CLI must
# match it exactly or the glue is incompatible. Read it from the pinned dep.
WB_VER="$(sed -n 's/^wasm-bindgen = "=\(.*\)"/\1/p' "crates/$CRATE/Cargo.toml")"
[ -n "$WB_VER" ] || { echo "could not read wasm-bindgen version from crates/$CRATE/Cargo.toml" >&2; exit 1; }

if ! command -v wasm-bindgen >/dev/null 2>&1; then
  echo "error: wasm-bindgen-cli not found. Install the matching version:" >&2
  echo "       cargo install wasm-bindgen-cli --version $WB_VER" >&2
  exit 1
fi
HAVE_VER="$(wasm-bindgen --version | awk '{print $2}')"
if [ "$HAVE_VER" != "$WB_VER" ]; then
  echo "error: wasm-bindgen-cli $HAVE_VER != crate $WB_VER (version skew breaks the glue)." >&2
  echo "       cargo install wasm-bindgen-cli --version $WB_VER --force" >&2
  exit 1
fi

echo "==> [$CRATE] cargo build (wasm32, size-optimized)"
# Size knobs for the artifact only (kept out of the workspace profile so other
# builds are unaffected): opt-for-size, abort on panic (no unwind tables), strip.
CARGO_PROFILE_RELEASE_OPT_LEVEL=z \
CARGO_PROFILE_RELEASE_PANIC=abort \
CARGO_PROFILE_RELEASE_STRIP=true \
CARGO_PROFILE_RELEASE_LTO=true \
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
  cargo build --release --target wasm32-unknown-unknown -p "$CRATE"

echo "==> [$CRATE] wasm-bindgen ($WB_VER) -> $OUT"
rm -rf "$OUT"
mkdir -p "$OUT"
# --target web: an ESM module with an `init(input?)` plus the named exports;
# works in the browser and (via the TS wrapper passing bytes) in Node.
wasm-bindgen --target web --out-dir "$OUT" --out-name "$OUTNAME" \
  "$PROFILE_DIR/$OUTNAME.wasm"

WASM="$OUT/${OUTNAME}_bg.wasm"
if command -v wasm-opt >/dev/null 2>&1; then
  echo "==> [$CRATE] wasm-opt -Oz"
  wasm-opt -Oz "$WASM" -o "$WASM.opt" && mv "$WASM.opt" "$WASM"
else
  echo "==> wasm-opt not found (binaryen) — skipping the -Oz pass" >&2
fi

RAW=$(stat -c%s "$WASM")
GZ=$(gzip -9 -c "$WASM" | wc -c)
printf '==> done: %s  %d bytes (%.0f KB),  gzip %d bytes (%.0f KB)\n' \
  "$WASM" "$RAW" "$(echo "$RAW/1024" | bc)" "$GZ" "$(echo "$GZ/1024" | bc)"
