// Public entry point for @retrochains/bcc — an ergonomic TypeScript API over the
// WASM build of the Rust Borland C++ 2.0 toolchain (bcc / tasm / tlink / tlib).
//
// The .wasm + wasm-bindgen glue under ../wasm is a build artifact produced by
// scripts/build-wasm.sh (run `npm run build:wasm`). The flat Rust boundary
// (crates/bcc-wasm) is wrapped here in object-style calls with sensible defaults.
import init, * as wasm from "../wasm/bcc_wasm.js";

/** BCC memory model (`-mt/-ms/-mc/-mm/-ml/-mh`). */
export type MemoryModel = "tiny" | "small" | "compact" | "medium" | "large" | "huge";

/** Options for {@link compile} / {@link compileAsm}, mirroring the `bcc` flags. */
export interface CompileOptions {
  /** Lowercase source name recorded in the OBJ `THEADR` (default `hello.c`). */
  filename?: string;
  /** Memory model (default `small`). */
  model?: MemoryModel;
  /** Source mtime as Unix seconds, embedded in the OBJ (default `0` — match a
   * `faketime`-pinned oracle build for byte-exactness). */
  mtimeUnix?: number;
  /** Merge duplicate string literals (`-d`). */
  mergeStrings?: boolean;
  /** Treat `char` as unsigned (`-K`). */
  unsignedChars?: boolean;
  /** Enable the optimizer (`-O`). */
  optimize?: boolean;
  /** Target the 80186/286 (`-1`). */
  target186?: boolean;
  /** Emit stack-overflow checks (`-N`). */
  stackCheck?: boolean;
  /** Suppress SI/DI register variables (`-r-`). */
  noRegVars?: boolean;
  /** Preprocessor defines as `NAME=VALUE` (or bare `NAME`) strings (`-D`). */
  defines?: string[];
}

/** A named blob — an object or library passed to {@link link} / {@link makeLibrary}. */
export interface NamedBytes {
  name: string;
  bytes: Uint8Array;
}

let ready: Promise<void> | undefined;

/** Instantiate the WASM module once (idempotent). Called lazily by every API. */
async function ensure(): Promise<void> {
  if (!ready) {
    const url = new URL("../wasm/bcc_wasm_bg.wasm", import.meta.url);
    const isNode = typeof process !== "undefined" && process.versions?.node != null;
    if (isNode) {
      const { readFile } = await import("node:fs/promises");
      const bytes = await readFile(url);
      ready = init({ module_or_path: bytes }).then(() => undefined);
    } else {
      ready = init({ module_or_path: url }).then(() => undefined);
    }
  }
  return ready;
}

/** Spread the flat positional argument list the WASM boundary expects. */
function flags(
  o: CompileOptions,
): [string, string, number, boolean, boolean, boolean, boolean, boolean, boolean, string[]] {
  return [
    o.filename ?? "hello.c",
    o.model ?? "small",
    o.mtimeUnix ?? 0,
    o.mergeStrings ?? false,
    o.unsignedChars ?? false,
    o.optimize ?? false,
    o.target186 ?? false,
    o.stackCheck ?? false,
    o.noRegVars ?? false,
    o.defines ?? [],
  ];
}

/** Compile C source to an OMF object file (`bcc -c`). */
export async function compile(source: string, options: CompileOptions = {}): Promise<Uint8Array> {
  await ensure();
  return wasm.compile(source, ...flags(options));
}

/** Compile C source to assembly text (`bcc -S`). */
export async function compileAsm(source: string, options: CompileOptions = {}): Promise<string> {
  await ensure();
  return wasm.compile_asm(source, ...flags(options));
}

/** Assemble TASM-syntax assembly to an OMF object file (`tasm`). */
export async function assemble(source: string): Promise<Uint8Array> {
  await ensure();
  return wasm.assemble(source);
}

/** Assemble with an explicit BCC memory-model marker COMENT byte. */
export async function assembleWithModel(source: string, modelMarker: number): Promise<Uint8Array> {
  await ensure();
  return wasm.assemble_with_model(source, modelMarker);
}

/** Link objects (and libraries) to an MZ `.EXE` image (`tlink`). */
export async function link(
  objects: NamedBytes[],
  libraries: NamedBytes[] = [],
): Promise<Uint8Array> {
  await ensure();
  const linker = new wasm.Linker();
  try {
    for (const o of objects) linker.add_object(o.name, o.bytes);
    for (const l of libraries) linker.add_library(l.name, l.bytes);
    return linker.link();
  } finally {
    linker.free();
  }
}

/** Build an OMF library archive from objects (`tlib`). */
export async function makeLibrary(objects: NamedBytes[], extended = false): Promise<Uint8Array> {
  await ensure();
  const librarian = new wasm.Librarian();
  try {
    for (const o of objects) librarian.add_object(o.name, o.bytes);
    return librarian.build(extended);
  } finally {
    librarian.free();
  }
}
