// Public entry point for @retrochains/decompile — a TypeScript API over the WASM
// build of the Rust decompiler + fingerprint analyzer. The module bundles NO
// compiler, so byte-exact round-trip verification is an app-level step: classify,
// decompile, then recompile with the matching compiler module (@retrochains/bcc).
//
// The .wasm + wasm-bindgen glue under ../wasm is a build artifact produced by
// scripts/build-wasm.sh (run `npm run build:wasm`).
import init, * as wasm from "../wasm/decompile_wasm.js";

let ready: Promise<void> | undefined;

/** Instantiate the WASM module once (idempotent). */
async function ensure(): Promise<void> {
  if (!ready) {
    const url = new URL("../wasm/decompile_wasm_bg.wasm", import.meta.url);
    const isNode = typeof process !== "undefined" && process.versions?.node != null;
    if (isNode) {
      const fsmod = "node:fs/promises";
      const { readFile } = await import(/* @vite-ignore */ fsmod);
      ready = init({ module_or_path: await readFile(url) }).then(() => undefined);
    } else {
      ready = init({ module_or_path: url }).then(() => undefined);
    }
  }
  return ready;
}

/** Which compiler produced some `_TEXT`, with the idiom evidence behind it. */
export interface Classification {
  /** `"bcc"`, `"msc"`, `"ambiguous"`, or `"unknown"`. */
  verdict: "bcc" | "msc" | "ambiguous" | "unknown";
  /** Count of BCC-distinctive idiom hits. */
  bccEvidence: number;
  /** Count of MSC-distinctive idiom hits. */
  mscEvidence: number;
  /** Total recognized idioms in the decomposition. */
  idiomCount: number;
}

/** Classify `_TEXT` machine code by which compiler produced it (idioms alone). */
export async function classify(code: Uint8Array): Promise<Classification> {
  await ensure();
  const c = wasm.classify(code);
  try {
    return {
      verdict: c.verdict as Classification["verdict"],
      bccEvidence: c.bcc_evidence,
      mscEvidence: c.msc_evidence,
      idiomCount: c.idiom_count,
    };
  } finally {
    c.free();
  }
}

/** Fraction of `code` bytes that lift to a recognized idiom (0.0–1.0). */
export async function coverage(code: Uint8Array): Promise<number> {
  await ensure();
  return wasm.coverage(code);
}

/** Pull the first CODE-class segment (`_TEXT`) out of an OMF object's bytes. */
export async function codeOfObj(obj: Uint8Array): Promise<Uint8Array> {
  await ensure();
  return wasm.code_of_obj(obj);
}

/**
 * Decompile a single function's `_TEXT` to compiler-accurate C, or `undefined` if
 * it isn't fully recovered. Verify it by recompiling with the matching compiler.
 */
export async function decompile(code: Uint8Array): Promise<string | undefined> {
  await ensure();
  return wasm.decompile(code) ?? undefined;
}

/**
 * Decompile a whole `_TEXT` segment (split into functions at the prologues), or
 * `undefined` if any function isn't fully recovered.
 */
export async function decompileProgram(code: Uint8Array): Promise<string | undefined> {
  await ensure();
  return wasm.decompile_program(code) ?? undefined;
}

/**
 * Why decompilation declined — the distinct proximate causes recovery hit, for
 * surfacing in a UI. Empty when {@link decompileProgram} succeeds. Each is an op
 * signature (`Bin:Mul`, `Load:deref`, `Asm(unlifted)`, …) or a structural tag
 * (`structure:*`, `program:globals`, …).
 */
export async function decompileReasons(code: Uint8Array): Promise<string[]> {
  await ensure();
  return wasm.decompile_reasons(code);
}
