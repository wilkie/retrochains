// The browser-side toolchain: turn a fixture's invocation into a byte-exact
// in-browser compile, verify it against the golden sha, and decompile + classify
// the result. The @retrochains/* WASM modules are imported lazily (inside the
// async ops) so the pure helpers below stay unit-testable without loading wasm.
import type { CompileOptions } from "@retrochains/bcc";
import type { Classification } from "@retrochains/decompile";
import type { Fixture, Family } from "./types";

export type { Classification };

/** Parse a BCC oracle command line into the options the wasm `compile` takes. */
export function parseBccArgs(args: string[]): CompileOptions {
  const opts: CompileOptions = {};
  const defines: string[] = [];
  for (const arg of args) {
    if (/\.c$/i.test(arg)) opts.filename = arg.toLowerCase();
    else if (arg === "-mt") opts.model = "tiny";
    else if (arg === "-ms") opts.model = "small";
    else if (arg === "-mc") opts.model = "compact";
    else if (arg === "-mm") opts.model = "medium";
    else if (arg === "-ml") opts.model = "large";
    else if (arg === "-mh") opts.model = "huge";
    else if (arg === "-K") opts.unsignedChars = true;
    else if (arg === "-O") opts.optimize = true;
    else if (arg === "-1") opts.target186 = true;
    else if (arg === "-N") opts.stackCheck = true;
    else if (arg === "-r-") opts.noRegVars = true;
    else if (arg === "-d") opts.mergeStrings = true;
    else if (arg.startsWith("-D")) defines.push(arg.slice(2));
  }
  if (defines.length) opts.defines = defines;
  return opts;
}

/** Lower-case hex sha-256 of a byte buffer (Web Crypto; works in Node 18+ too). */
export async function sha256(bytes: Uint8Array): Promise<string> {
  // Cast through `unknown`: TS 5.7's generic `Uint8Array<ArrayBufferLike>` isn't
  // assignable to `BufferSource`, but it's a valid argument at runtime.
  const digest = await crypto.subtle.digest("SHA-256", bytes as unknown as BufferSource);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export interface CompileResult {
  obj: Uint8Array;
  sha: string;
  /** sha matches the recorded golden — a byte-exact reproduction. */
  verified: boolean;
  /** Assembly text (BCC only). */
  asm?: string;
}

/** Compile a fixture's source with the matching module and verify vs the golden. */
export async function compileAndVerify(fixture: Fixture, family: Family): Promise<CompileResult> {
  if (family === "bcc") {
    const entry = fixture.bcc!;
    const { compile, compileAsm } = await import("@retrochains/bcc");
    const opts: CompileOptions = {
      ...parseBccArgs(entry.args),
      mtimeUnix: entry.fakeTimeUnix ?? 0,
    };
    const obj = await compile(fixture.source, opts);
    const asm = await compileAsm(fixture.source, opts);
    const sha = await sha256(obj);
    return { obj, sha, verified: sha === entry.objSha, asm };
  }
  const entry = fixture.msc!;
  const { compile } = await import("@retrochains/msc");
  const obj = await compile(fixture.source, "HELLO.C");
  const sha = await sha256(obj);
  return { obj, sha, verified: sha === entry.objSha };
}

export interface Analysis {
  /** The `_TEXT` segment pulled out of the OBJ. */
  code: Uint8Array;
  classification: Classification;
  /** Recovered C, or `undefined` if not fully decompilable. */
  decompiled: string | undefined;
}

/** Pull `_TEXT` from an OBJ, classify it, and decompile it back to C. */
export async function analyze(obj: Uint8Array): Promise<Analysis> {
  const { classify, codeOfObj, decompileProgram } = await import("@retrochains/decompile");
  const code = await codeOfObj(obj);
  const classification = await classify(code);
  const decompiled = await decompileProgram(code);
  return { code, classification, decompiled };
}
