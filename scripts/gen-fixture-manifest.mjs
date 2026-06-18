#!/usr/bin/env node
// Generate the fixture manifest the corpus explorer loads: walk fixtures/c, and
// for each fixture (a dir with HELLO.C) record its source plus, per compiler, the
// invocation args, the recorded golden OBJ sha256/size, and the faketime the
// oracle pinned. The browser recompiles the source with the matching WASM module
// and checks sha256(OBJ) against objSha — reproducing `verify` with no DOSBox.
//
// Sources total ~0.4 MB across the corpus, so inlining them keeps the explorer a
// single static fetch. Output: apps/explorer/public/fixtures.json
import { readFileSync, writeFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("..", import.meta.url));
const FIXTURES = join(ROOT, "fixtures", "c");
const OUT = join(ROOT, "apps", "explorer", "public", "fixtures.json");

/** Recursively list every directory that directly contains a HELLO.C. */
function fixtureDirs(dir, acc = []) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return acc;
  }
  if (entries.some((e) => e.isFile() && e.name === "HELLO.C")) acc.push(dir);
  for (const e of entries) if (e.isDirectory()) fixtureDirs(join(dir, e.name), acc);
  return acc;
}

/** Pull a TOML `key = [ ... ]` string array (single- or multi-line). */
function tomlArray(text, key) {
  const m = text.match(new RegExp(`${key}\\s*=\\s*\\[([\\s\\S]*?)\\]`));
  if (!m) return null;
  return [...m[1].matchAll(/['"]([^'"]*)['"]/g)].map((x) => x[1]);
}

function tomlString(text, key) {
  const m = text.match(new RegExp(`${key}\\s*=\\s*"([^"]+)"`));
  return m ? m[1] : null;
}

/** From an `expected/<fam>/<REL>.toml`, the `.OBJ` output's sha256 + size. */
function objOutput(text) {
  // Split on [[outputs]] and find the block whose name ends in .OBJ.
  for (const block of text.split("[[outputs]]").slice(1)) {
    const name = tomlString(block, "name");
    if (name && /\.OBJ$/i.test(name)) {
      const sha = tomlString(block, "sha256");
      const size = block.match(/size\s*=\s*(\d+)/);
      return sha ? { sha, size: size ? Number(size[1]) : null } : null;
    }
  }
  return null;
}

function firstToml(dir) {
  // expected/<fam>/ holds one release-keyed toml (e.g. BC2.toml / MSC500.toml).
  if (!existsSync(dir)) return null;
  const f = readdirSync(dir).find((n) => n.endsWith(".toml"));
  return f ? readFileSync(join(dir, f), "utf8") : null;
}

function compilerEntry(fixtureDir, family) {
  const invPath = join(fixtureDir, `invocation.${family}.toml`);
  if (!existsSync(invPath)) return null;
  const inv = readFileSync(invPath, "utf8");
  const args = tomlArray(inv, "args") ?? [];
  const expected = firstToml(join(fixtureDir, "expected", family));
  if (!expected) return null;
  const obj = objOutput(expected);
  if (!obj) return null;
  const fakeTime = tomlString(expected, "fake_time");
  return {
    args,
    objSha: obj.sha,
    objSize: obj.size,
    fakeTimeUnix: fakeTime ? Math.floor(Date.parse(fakeTime) / 1000) : 0,
  };
}

const fixtures = [];
for (const dir of fixtureDirs(FIXTURES).sort()) {
  const id = relative(FIXTURES, dir).split("\\").join("/");
  const [area = "misc", sub = "", ...rest] = id.split("/");
  const name = [sub, ...rest].filter(Boolean).join("/") || id;
  const bcc = compilerEntry(dir, "bcc");
  const msc = compilerEntry(dir, "msc");
  if (!bcc && !msc) continue;
  const source = readFileSync(join(dir, "HELLO.C"), "utf8");
  fixtures.push({ id, area, sub, name, source, ...(bcc && { bcc }), ...(msc && { msc }) });
}

const manifest = { generated: new Date().toISOString(), count: fixtures.length, fixtures };
writeFileSync(OUT, JSON.stringify(manifest));
const kb = (statSync(OUT).size / 1024).toFixed(0);
console.log(`wrote ${OUT}: ${fixtures.length} fixtures, ${kb} KB`);
