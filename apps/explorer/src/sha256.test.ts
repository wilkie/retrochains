import { createHash } from "node:crypto";
import { sha256Hex } from "./sha256";

const node = (b: Uint8Array) => createHash("sha256").update(b).digest("hex");

test("matches known vectors", () => {
  expect(sha256Hex(new Uint8Array(0))).toBe(
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  );
  expect(sha256Hex(new TextEncoder().encode("abc"))).toBe(
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  );
});

test("agrees with Node crypto across lengths spanning the block boundary", () => {
  for (const len of [0, 1, 55, 56, 63, 64, 65, 119, 120, 241, 1000]) {
    const bytes = new Uint8Array(len);
    for (let i = 0; i < len; i++) bytes[i] = (i * 37 + 13) & 0xff;
    expect(sha256Hex(bytes)).toBe(node(bytes));
  }
});
