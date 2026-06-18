// Classic `hexdump -C` style rendering of a byte buffer, for the OBJ/EXE view.

const HEX = Array.from({ length: 256 }, (_, i) => i.toString(16).padStart(2, "0"));

/** Render `bytes` as offset + 16 hex columns + ASCII gutter, one line per 16. */
export function hexdump(bytes: Uint8Array, maxBytes = 4096): string {
  const n = Math.min(bytes.length, maxBytes);
  const lines: string[] = [];
  for (let off = 0; off < n; off += 16) {
    const row = bytes.subarray(off, Math.min(off + 16, n));
    const hex: string[] = [];
    let ascii = "";
    for (let i = 0; i < 16; i++) {
      const b = row[i];
      if (b === undefined) {
        hex.push("  ");
      } else {
        hex.push(HEX[b]!);
        ascii += b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : ".";
      }
      if (i === 7) hex.push("");
    }
    lines.push(`${off.toString(16).padStart(8, "0")}  ${hex.join(" ")}  |${ascii}|`);
  }
  if (bytes.length > n) lines.push(`… ${bytes.length - n} more bytes`);
  return lines.join("\n");
}
