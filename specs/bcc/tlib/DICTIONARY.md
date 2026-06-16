# TLIB dictionary hashing

How Turbo Librarian 2.0 hashes symbol names into the `.LIB` dictionary — the
symbol→page index TLINK uses to locate a defining member without scanning every
object. Reverse-engineered empirically against TLIB-built archives (probe libs
built with the now-shipped `TLIB.EXE`; see `../../formats/LIB_ARCHIVE.md` for the
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

Confirmed on 24+ collision-free single-symbol archives — e.g. `A`→23, `P`→1,
`AB`→0x8079→33, `ADDONE`→16. The "lowercasing" is what makes a single char `X`
land at `(X | 0x20) % 37` (e.g. `A`=0x41→0x61=97→23). Case is irrelevant: TLIB's
index is case-insensitive.

Two corroborations rule out simpler hashes:

- **`| 0x20` is unconditional**, applied to *every* byte, not a real
  `tolower()`. Probed with identifier chars where the two differ: `@` (0x40 →
  0x60) and `_` (0x5F → 0x7F). TLIB puts `A@` at bucket 10 and `A_` at 31 —
  exactly the `| 0x20` values; an alpha-only lowercase would give 2 and 23.
- **It is a full scan**, not a bounded first/last-word hash like BCC's *internal*
  symbol table (`hash.py`: `count<<6 + first_word + last_word<<3`, mod 0x400).
  Changing a *middle* character with the same first/last word changes the
  bucket: `ABCDE`→30 vs `ABXDE`→21; `ABCDEF`→15 vs `ABZZEF`→11.

## Insertion order & members

- Symbols are inserted in **sorted (ASCII) order**. On a collision the
  alphabetically-earlier name keeps its primary bucket; the later name is
  rehashed. Verified by every collision pair in a 33-entry probe (`AC`<`CBA`<`D`
  all hash to bucket 26 → `AC` keeps 26).
- Each member contributes its **public symbols** plus its **module name with a
  trailing `!`** (`ADD` → `ADD!`), hashed by the same function.

## The full algorithm (from TLIB.EXE disassembly)

`TLIB.EXE`'s hash routine (BC2, file offset `0x24aa`) computes **all four**
values in one pass, confirming and completing the probe results. The decisive
detail — only visible in the code — is that the name is a **Pascal
(length-prefixed) string** and the two scan pointers cover *different* ranges:

- the **backward** pointer starts at `name + len` and walks down to `name[1]`,
  so it folds `charL, …, char1` — every char, reversed, **without** the length
  byte;
- the **forward** pointer starts at `name[0]` — the **length byte** — and the
  loop runs `len` times, so it folds `[len, char1, …, char(len-1)]` — the length
  byte plus every char **except the last**.

Each byte is folded `acc = rotate(acc, 2) ^ (byte | 0x20)` (the `| 0x20` hits
the length byte too). The four results:

| value        | bytes folded            | rotate | reduce                |
|--------------|-------------------------|--------|-----------------------|
| bucket index | `charL … char1`         | `ror`  | `% 37`                |
| bucket delta | `[len] char1 … char(L-1)`| `ror` | `% 37`, →1 if 0       |
| block index  | `[len] char1 … char(L-1)`| `rol` | `% nblocks`           |
| block delta  | `charL … char1`         | `rol`  | `% nblocks`, →1 if 0  |

On a collision: `bucket = (bucket + bucket_delta) % 37`, retried until free; if
the probe cycles back to the original bucket the block advances by `block_delta`
(`block = (block + block_delta) % nblocks`).

This explains the probe puzzles exactly:

- The **bucket delta ignores the last char** because the forward scan stops one
  short of it — `AD`/`AE`/`AF` all fold `[2, 'A']` → 17.
- A **single char's delta is constant 33** because the forward scan then folds
  only the length byte: `ror2(0) ^ (0x01 | 0x20) = 0x21 = 33`.

Both the delta and `g(prefix)` formulas reproduce **every** measured data point
(verified in `dict.rs` tests). The earlier "`g(c) = c-0x30-0x10·bit3`" was a
correct-but-opaque fit to this; the real form is the length-byte fold above.

Reproduced in `crates/bcc-tlib/src/dict.rs` (`bucket`, `bucket_delta`, `block`,
`block_delta`). With sorted insertion + the block layout, the dictionary is now
fully determined — including colliding and multi-block libraries (the latter
still wants one captured ≥2-block lib as a belt-and-suspenders check, but the
code is unambiguous).
