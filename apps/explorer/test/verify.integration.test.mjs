// Integration test: the explorer's headline feature — recompile a fixture's
// source in-(node-)browser with the WASM modules and confirm it reproduces the
// recorded golden OBJ sha256, byte-for-byte. Exercises the same code path the
// browser uses. Requires the @retrochains packages + the manifest to be built:
//   pnpm -r run build:ts && node ../../scripts/gen-fixture-manifest.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { compile as bccCompile } from "@retrochains/bcc";
import { compile as mscCompile } from "@retrochains/msc";
import { codeOfObj, classify, decompile, decompileProgram, decompileReasons } from "@retrochains/decompile";

const manifest = JSON.parse(
  readFileSync(new URL("../public/fixtures.json", import.meta.url), "utf8"),
);
const sha = (b) => createHash("sha256").update(b).digest("hex");

const MODELS = { "-mt": "tiny", "-ms": "small", "-mc": "compact", "-mm": "medium", "-ml": "large", "-mh": "huge" };
function bccOpts(entry) {
  const o = { mtimeUnix: entry.fakeTimeUnix ?? 0, defines: [] };
  for (const a of entry.args) {
    if (/\.c$/i.test(a)) o.filename = a.toLowerCase();
    else if (MODELS[a]) o.model = MODELS[a];
    else if (a === "-K") o.unsignedChars = true;
    else if (a === "-O") o.optimize = true;
    else if (a === "-1") o.target186 = true;
    else if (a === "-N") o.stackCheck = true;
    else if (a === "-r-") o.noRegVars = true;
    else if (a === "-d") o.mergeStrings = true;
    else if (a.startsWith("-D")) o.defines.push(a.slice(2));
  }
  return o;
}

const SAMPLE = 30;

test("BCC fixtures recompile byte-exactly to the golden OBJ", async () => {
  const sample = manifest.fixtures.filter((f) => f.bcc).slice(0, SAMPLE);
  assert.ok(sample.length > 0, "have bcc fixtures");
  for (const f of sample) {
    const obj = await bccCompile(f.source, bccOpts(f.bcc));
    assert.equal(sha(obj), f.bcc.objSha, `bcc ${f.id}`);
  }
});

test("MSC fixtures recompile byte-exactly to the golden OBJ", async () => {
  const sample = manifest.fixtures.filter((f) => f.msc).slice(0, SAMPLE);
  assert.ok(sample.length > 0, "have msc fixtures");
  for (const f of sample) {
    const obj = await mscCompile(f.source, "HELLO.C");
    assert.equal(sha(obj), f.msc.objSha, `msc ${f.id}`);
  }
});

test("classify + decompile run over a compiled fixture's _TEXT", async () => {
  const f = manifest.fixtures.find((x) => x.bcc && x.id.includes("2478-double-not"));
  assert.ok(f, "found the sample fixture");
  const obj = await bccCompile(f.source, bccOpts(f.bcc));
  const code = await codeOfObj(obj);
  assert.ok(code.length > 0, "extracted _TEXT");
  const c = await classify(code);
  assert.equal(c.verdict, "bcc", "classifies as BCC");
  // decompile may or may not fully recover; just assert it doesn't throw.
  await decompile(code);
});

test("a declined decompile reports its bail reasons", async () => {
  // `long` negation isn't recovered yet — recovery bails on the `neg` (Un:Neg),
  // which the explorer surfaces in place of the generic "not decompilable".
  const obj = await bccCompile("long f(){ long x = 5; return -x; }\n", {
    filename: "hello.c",
    model: "small",
    mtimeUnix: 672408000,
  });
  const code = await codeOfObj(obj);
  assert.equal(await decompileProgram(code), undefined, "declines");
  const reasons = await decompileReasons(code);
  assert.ok(reasons.length > 0, "surfaces at least one reason");
  assert.ok(
    reasons.every((r) => typeof r === "string" && r.length > 0),
    "reasons are non-empty strings",
  );
});
