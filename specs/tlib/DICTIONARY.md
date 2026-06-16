# TLIB dictionary hashing

How Turbo Librarian 2.0 hashes symbol names into the `.LIB` dictionary — the
symbol→page index TLINK uses to locate a defining member without scanning every
object. Reverse-engineered empirically against TLIB-built archives (probe libs
built with the now-shipped `TLIB.EXE`; see `../formats/LIB_ARCHIVE.md` for the
surrounding archive framing). Reproduced in `crates/bcc-tlib/src/dict.rs`.

## Block layout

A dictionary is one or more **512-byte blocks**. Within a block:

- bytes `0..37` — the **bucket table**: `htab[b]` holds the entry offset (÷2)
  for bucket `b`, or `0` if the bucket is empty. 37 buckets per block.
- byte `37` — the **free-space pointer** (÷2): where the next entry is written.
- bytes `38..` — **entries**, packed upward on even offsets, each
  `<len:u8> <name bytes> <page:u16-le>` where `page` is the 1-based archive page
  the defining member starts on.

## The bucket hash (confirmed)

The bucket index is a 16-bit accumulator folded over the name **back to front**,
**lowercasing** each byte (`c | 0x20`):

```
H = 0
for c in reversed(name):
    H = ror16(H, 2) ^ (c | 0x20)     // rotate the u16 right by 2, then xor
bucket = H % 37
```

Confirmed on 16 collision-free single-symbol archives — e.g. `A`→23, `P`→1,
`AB`→0x8079→33, `ADDONE`→16. The "lowercasing" is what makes a single char `X`
land at `(X | 0x20) % 37` (e.g. `A`=0x41→0x61=97→23). Case is irrelevant: TLIB's
index is case-insensitive.

## Insertion order & members

- Symbols are inserted in **sorted (ASCII) order**. On a collision the
  alphabetically-earlier name keeps its primary bucket; the later name is
  rehashed. Verified by every collision pair in a 33-entry probe (`AC`<`CBA`<`D`
  all hash to bucket 26 → `AC` keeps 26).
- Each member contributes its **public symbols** plus its **module name with a
  trailing `!`** (`ADD` → `ADD!`), hashed by the same function.

## Open — not yet reverse-engineered

- **Collision rehash delta.** On a taken bucket TLIB advances by a per-symbol
  step (`bucket = (bucket + delta) % 37`, repeated). The step isn't the mirror
  forward-rol hash; single-collision observations (`D` needs +25 over an `AC` at
  26, `P` +1, `YX` +2) don't yet fit a single model. Needs controlled
  two-symbol forced-collision archives to isolate `delta(name)`.
- **Block index** (multi-block dictionaries). All probes so far fit one 512-byte
  block (`block = 0`). A library large enough to need ≥2 blocks is required to
  derive the block hash and block-rehash delta.

These gate byte-exact reproduction of *colliding* / *large* libraries; small
collision-free archives (e.g. fixture 4262's `MYLIB.LIB`) are fully determined
by the bucket hash + sorted insertion + the layout above.
