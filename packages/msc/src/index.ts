// Public entry point for @retrochains/msc — a TypeScript API over the WASM build
// of the Rust MSC compiler reimplementation. MSC is compile-only here (small
// model, `cl /c /AS`); there is no MSC linker/librarian.
//
// The .wasm + wasm-bindgen glue under ../wasm is a build artifact produced by
// scripts/build-wasm.sh (run `npm run build:wasm`).
import init, * as wasm from "../wasm/msc_wasm.js";

let ready: Promise<void> | undefined;

/** Instantiate the WASM module once (idempotent). */
async function ensure(): Promise<void> {
  if (!ready) {
    const url = new URL("../wasm/msc_wasm_bg.wasm", import.meta.url);
    const isNode = typeof process !== "undefined" && process.versions?.node != null;
    if (isNode) {
      const { readFile } = await import("node:fs/promises");
      ready = init({ module_or_path: await readFile(url) }).then(() => undefined);
    } else {
      ready = init({ module_or_path: url }).then(() => undefined);
    }
  }
  return ready;
}

/**
 * Compile C source to an OMF object file (`cl /c /AS`). `filename` is the source
 * name recorded in the object (default `HELLO.C`).
 */
export async function compile(source: string, filename = "HELLO.C"): Promise<Uint8Array> {
  await ensure();
  return wasm.compile(source, filename);
}
