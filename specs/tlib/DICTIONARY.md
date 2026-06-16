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

## Collision rehash delta (partially characterized)

On a taken bucket TLIB advances by a per-symbol step: `bucket =
(bucket + delta(name)) % 37`, repeated until free. Forced-collision probes (a
filler symbol planted on the primary bucket, the target rehashing once) give
clean single-step deltas. Structure found so far:

- **The delta ignores the *last* character**: `delta(name) = g(name[:-1])`.
  Evidence: `AD`, `AE`, `AF` all give delta 17 (vary only the last char); a
  single char gives a constant delta **33** (its prefix is empty, so
  `g("") = 33`).
- **`g` of a single prefix char** `c` is `(c & 0x0F) | 0x10`:
  `g("A")=17, g("B")=18 … g("F")=22, g("X")=24, g("Y")=25`.

Clean delta data (target → delta), for fitting the full `g`:

| len | observations |
|----|---------------|
| 1  | `D`,`Q`,`R`,`S` → 33 |
| 2  | `AD`/`AE`/`AF`→17, `BA`→18, `CA`→19, `DA`→20, `EA`→21, `FA`→22, `XY`→24, `YX`→25 |
| 3  | `CBA`→29, `HHH`→5 |
| 4  | `MAAA`→16 |
| 5  | `ABCDE`→2 |

The full `g` (a hash over `name[:-1]`) isn't pinned yet — it isn't the mirror
forward-rotate of the bucket hash, and the `(c&0xF)|0x10` single-char form is
unusual. Next: probe `g` over 2- and 3-char prefixes systematically.

## Open — block index (multi-block dictionaries)

All probes so far fit one 512-byte block (`block = 0`). A library large enough
to need ≥2 blocks is required to derive the block hash and block-rehash delta.

These gate byte-exact reproduction of *colliding* / *large* libraries; small
collision-free archives (e.g. fixture 4262's `MYLIB.LIB`) are fully determined
by the bucket hash + sorted insertion + the layout above.
