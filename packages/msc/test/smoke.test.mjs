// Smoke test: drive the MSC WASM compiler and check the bytes look like real OMF.
// Requires the package to be built first (`npm run build`). Run with `npm test`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { compile } from "../dist/index.js";

const HELLO = "int main() { return 0; }\n";

test("compile produces an OMF object (THEADR-led, deterministic)", async () => {
  const obj = await compile(HELLO, "HELLO.C");
  assert.ok(obj instanceof Uint8Array && obj.length > 0, "non-empty OBJ");
  // OMF object files begin with an 0x80 THEADR record.
  assert.equal(obj[0], 0x80, "first record is THEADR (0x80)");
  const again = await compile(HELLO, "HELLO.C");
  assert.deepEqual([...again], [...obj], "compilation is deterministic");
});
