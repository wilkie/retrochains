// Smoke test: drive the WASM toolchain end-to-end and check the bytes look like
// real OMF/EXE output. Requires the package to be built first (`npm run build`,
// which builds the WASM and compiles dist/). Run with `npm test` (node --test).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  compile,
  compileAsm,
  assemble,
  makeLibrary,
} from "../dist/index.js";

const HELLO = "int main() { return 0; }\n";

test("compile produces an OMF object (THEADR-led, deterministic)", async () => {
  const obj = await compile(HELLO, { filename: "hello.c" });
  assert.ok(obj instanceof Uint8Array && obj.length > 0, "non-empty OBJ");
  // OMF object files begin with an 0x80 THEADR record.
  assert.equal(obj[0], 0x80, "first record is THEADR (0x80)");
  // Same inputs must reproduce the same bytes (byte-exact determinism).
  const again = await compile(HELLO, { filename: "hello.c" });
  assert.deepEqual([...again], [...obj], "compilation is deterministic");
});

test("compileAsm produces .ASM text", async () => {
  const asm = await compileAsm(HELLO, { filename: "hello.c" });
  assert.equal(typeof asm, "string");
  assert.ok(asm.length > 0, "non-empty ASM");
});

test("assemble round-trips the compiler's own .ASM to an OBJ", async () => {
  const asm = await compileAsm(HELLO, { filename: "hello.c" });
  const obj = await assemble(asm);
  assert.ok(obj instanceof Uint8Array && obj.length > 0);
  assert.equal(obj[0], 0x80, "assembled OBJ is THEADR-led");
});

test("tlib archives an object into a library", async () => {
  const obj = await compile(HELLO, { filename: "hello.c" });
  const lib = await makeLibrary([{ name: "hello.obj", bytes: obj }]);
  assert.ok(lib instanceof Uint8Array && lib.length > 0, "non-empty LIB");
  // OMF libraries begin with an 0xF0 LIBHDR record.
  assert.equal(lib[0], 0xf0, "first record is LIBHDR (0xF0)");
});
