# TLINK capstone — linking real BCC output byte-exact

The end-to-end proof that `crates/bcc-tlink` reproduces TLINK: take a real
program compiled by BCC, link it against the **real** Borland C++ 2.0 startup
(`C0S.OBJ`) and runtime library (`CS.LIB`), and match TLINK's `MAIN.EXE` and
`MAIN.MAP` byte-for-byte.

```
tlink /m C0S.OBJ+MAIN.OBJ, MAIN.EXE, MAIN.MAP, CS.LIB
```

Two programs are gated by `crates/bcc-tlink/tests/capstone.rs`:

- `MAIN.C` — `int main(void){return 0;}` (`bcc -c -ms`). Pulls **16 members**
  transitively from CS.LIB; 16 segments, 140-odd publics, runtime relocations.
- `HELLO.C` — `printf("Hello, world\n")` (`bcc -c -ms -IC:\INCLUDE`). Pulls the
  formatted-output / stdio chain and a real `_DATA` string constant — much more
  of CS.LIB, more fixup variety.

The test reads `C0S.OBJ`/`CS.LIB` from the provisioned install (`.bc2/BC2/LIB/`,
reproducible via `oracle provision bcc`, so not tracked) and asserts the
recorded SHA-256 of the linked `MAIN.EXE`/`MAIN.MAP`. It skips cleanly when the
install is absent. Only the small object files are tracked (`tests/data/`).

Closing this required a handful of behaviors the small standalone fixtures
never exercised. Each is now in `omf.rs`/`link.rs`/`map.rs`.

## OMF parsing

- **Absolute PUBDEF** — a public with base group 0 *and* base segment 0 carries
  a 16-bit **Base Frame** before the symbol list. These are absolute equates
  (`__AHSHIFT = 0x000C`, `__AHINCR = 0x1000`, the huge-pointer constants).
  Their value is the constant `frame:offset`, tied to no segment. Missing the
  frame field mis-frames the record and the next byte is read as a name length
  (`truncated name payload`).
- **LIDATA** (`0xA2`) — iterated/repeated data. C0's `_DATA` uses it to lay
  down a zero run. Each block is `repeat:u16, block_count:u16`; `block_count==0`
  means a length-prefixed literal run follows, otherwise nested blocks. Expand
  recursively into concrete bytes and write them like LEDATA.

## Segment layout (`link.rs`)

TLINK's C0 startup carries the `DOSSEG` linker directive (a COMENT, class
`0xA1`). Two rules reproduce its layout:

- **Order — group by class.** Segments are emitted grouped by their class, with
  classes in first-appearance order and segments within a class in
  first-appearance order. So C0's `_DATA, _CVTSEG, _SCNSEG` (all class `DATA`)
  stay together, and `_CONST` (class `CONST`) follows the whole `DATA` run even
  though its SEGDEF appears before `_CVTSEG`.
- **Alignment — honor own alignment; paragraph-align only a group's first
  member.** Every segment is placed at its own SEGDEF alignment (byte/word/para)
  and packs against the previous one. The *one* exception: the **first member of
  a group** starts on a fresh paragraph, because a group base (DGROUP) must sit
  on a paragraph boundary to be a valid frame. So DGROUP's interior segments
  butt together (`_DATA` ends at `0xCE3`, `_CVTSEG` starts at `0xCE4`, not
  `0xCF0`), the standalone fixture 4261's lone `_DATA` paragraph-aligns (it is
  the first DGROUP member), and — crucially for medium/large model — per-module
  CODE segments, which are *not* grouped, byte-pack against each other
  (`MAIN_TEXT` at `0xE6F`, odd, right after the previous code segment).

## Memory models

In medium/compact/large/huge, code and/or data go *far*: each module gets its
own code segment `<MODULE>_TEXT` (class CODE) instead of a single shared
`_TEXT`, and calls/pointers across them are far (location-type-3 fixups +
runtime relocations). The startup object and runtime library are model-keyed
(`C0M.OBJ`/`CM.LIB`, `C0L.OBJ`/`CL.LIB`, …). Two things fall out:

- **Byte-packed code.** The per-module CODE segments share a class but no group,
  so by the alignment rule above they pack at their own (byte) alignment — a
  code segment commonly starts on an odd paragraph-plus address.
- **Relocation offset carries the sub-paragraph remainder.** A runtime
  relocation is `(frame, offset)` with `frame = segment_load >> 4`. When the
  patched segment is byte-packed, `segment_load` isn't a paragraph multiple, so
  the offset must be `offset_in_segment + (segment_load & 15)` for `frame*16 +
  offset` to land on the patched word. (Paragraph-aligned segments — every
  small-model case — have remainder 0, which is why this only surfaced with far
  models.)

Both `return 0` and `printf` link byte-exact — EXE *and* `.MAP` — in small,
medium, and large models.

## Group-relative framing (publics *and* fixups)

A reference to a segment that belongs to a group is framed against the **group
base paragraph**, not the segment's own paragraph. This shows up in two places:

- **`.MAP` publics** — C0's `__ATEXITTBL` lands in `_BSS` (paragraph `0x0CE`)
  but the map shows `00A6:028A`: frame `0xA6` is DGROUP's base, offset `0x28A`
  into the group.
- **Fixups** — `printf` needs it. The `REALCVT` member has a near, seg-relative,
  F5-framed (frame = target's frame) fixup to the external `__RealCvtVector`,
  which lives in `_CVTSEG` (part of DGROUP). Framed against `_CVTSEG`'s own
  paragraph the deposited offset would be `0x000C`; framed against DGROUP (the
  correct frame) it is `0x02FC`. So the linker carries a per-combined-segment
  *canonical frame* (group base where grouped, own paragraph otherwise) and uses
  it for both T4/T6 target frames and F4 location frames.

## `.MAP` rendering (`map.rs`)

- **Uppercase** — TLINK upper-cases every public name in the listing
  (`_main` → `_MAIN`, `__C0argc` → `__C0ARGC`).
- **`Abs` tag** — absolute equates render `  Abs  ` in the 7-column gap after
  the address instead of blanks.
- **Publics by Value ordering** — absolutes group **first** (sorted by offset),
  then relocatable symbols by `(frame, offset)`. Ties break by **symbol-table
  insertion order**: the order in which TLINK first *enters* each name. A name
  is entered the first time it's seen scanning modules in link order — as an
  external reference (EXTDEF) *or* a definition (PUBDEF), whichever comes first.
  Two consequences, both verified byte-exact:
  - `__ScanTodVector` (entered first) precedes `__RealCvtVector` at the shared
    `00A6:0284` — both only defined, in PUBDEF order.
  - `_free` precedes `_farfree` at `0000:0B30` in the large model even though
    FARHEAP defines `_farfree`'s PUBDEF first — because `_free` is *referenced*
    (EXTDEF) by the runtime well before FARHEAP is pulled, so it's entered
    earlier. Counting PUBDEFs alone gets this backwards.

  This isn't a hashing artifact. A controlled probe (a TASM module with 26
  same-address publics `SYA`..`SYZ`) showed the by-value order matching the
  OBJ's PUBDEF record order exactly — TLINK preserves insertion order; the
  "scrambled" look comes from TASM's own PUBDEF emission order.

## Library member placement (`lib.rs`)

The pulled members land in the output in **library order** (ascending member
index), independent of the order resolution discovered them. For this link the
pulled members are CS.LIB indices `2, 31, 35, 41, 51, 95, 96, 128, 143, 147,
159, 196, 198, 204, 258, 289` — strictly ascending — and that is the order
their `_TEXT`/`_DATA`/`_BSS` contributions concatenate. (Resolution still pulls
on demand to find the transitive set; only the final placement is re-sorted by
library index.)
