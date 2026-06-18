import { hexdump } from "./hexdump";

test("renders offset, hex columns, and the ascii gutter", () => {
  const out = hexdump(new Uint8Array([0x80, 0x41, 0x42, 0x00]));
  expect(out).toContain("00000000");
  expect(out).toContain("80 41 42 00");
  expect(out).toContain("|.AB.|");
});

test("caps output and notes the remaining byte count", () => {
  const out = hexdump(new Uint8Array(40), 16);
  expect(out).toContain("24 more bytes");
});
