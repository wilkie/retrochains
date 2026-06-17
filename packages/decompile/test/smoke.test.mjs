// Smoke test: drive the decompiler + fingerprint over a known BCC function's
// _TEXT. Requires the package to be built first (`npm run build`). Run `npm test`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { classify, coverage, decompile } from "../dist/index.js";

// `int f(int x){ return x + 1; }` compiled by our byte-exact BCC:
//   push bp; mov bp,sp; mov ax,[bp+4]; inc ax; jmp $+2; pop bp; ret
// The `eb 00` (jmp $+2) is a BCC-distinctive exit-jump idiom.
const TEXT = new Uint8Array([0x55, 0x8b, 0xec, 0x8b, 0x46, 0x04, 0x40, 0xeb, 0x00, 0x5d, 0xc3]);

test("classify recognizes BCC codegen", async () => {
  const c = await classify(TEXT);
  assert.equal(c.verdict, "bcc", "verdict is BCC");
  assert.ok(c.bccEvidence > 0, "has BCC-distinctive evidence");
  assert.ok(c.idiomCount > 0, "recognized some idioms");
});

test("coverage is a fraction in [0,1]", async () => {
  const cov = await coverage(TEXT);
  assert.ok(cov >= 0 && cov <= 1, `coverage ${cov} in range`);
});

test("decompile recovers the function body", async () => {
  const c = await decompile(TEXT);
  assert.equal(typeof c, "string", "recovered C");
  assert.match(c, /return\s*\(?p1\s*\+\s*1\)?/, "recovered the x+1 return");
});
