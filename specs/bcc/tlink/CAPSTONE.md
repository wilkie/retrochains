# TLINK capstone — linking real BCC output byte-exact

The end-to-end proof that `crates/bcc-tlink` reproduces TLINK: take a real
program compiled by BCC, link it against the **real** Borland C++ 2.0 startup
(`C0S.OBJ`) and runtime library (`CS.LIB`), and match TLINK's `MAIN.EXE` and
`MAIN.MAP` byte-for-byte.

```
tlink /m C0S.OBJ+MAIN.OBJ, MAIN.EXE, MAIN.MAP, CS.LIB
```

`MAIN.C` is `int main(void){return 0;}` (`bcc -c -ms`). Linking pulls **16
members** transitively from CS.LIB. The result: 16 segments, 140-odd publics,
runtime relocations — all byte-exact. Gated by the integration test
`crates/bcc-tlink/tests/capstone.rs`, which reads `C0S.OBJ`/`CS.LIB` from the
provisioned install (`.bc2/BC2/LIB/`, reproducible via `oracle provision bcc`,
so not tracked) and asserts the recorded SHA-256 of the linked outputs. It
skips cleanly when the install is absent.

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
- **Alignment — pack within a group, paragraph between frames.** A segment that
  continues the *same group* as the immediately-preceding segment is placed at
  its own SEGDEF alignment (byte/word/para), packing tight — DGROUP's interior
  segments butt against each other (`_DATA` ends at `0xCE3`, `_CVTSEG` starts at
  `0xCE4`, *not* `0xCF0`). Every other segment — the first member of a group, or
  any ungrouped segment — starts on a fresh paragraph so it owns a clean frame.
  (The single-DGROUP-member standalone fixture 4261 depends on this: its
  `_DATA` paragraph-aligns because it is the first DGROUP member.)

## Public framing (`.MAP`)

A public defined in a grouped segment is reported relative to the **group base
paragraph**, not its own segment's paragraph. C0's `__ATEXITTBL` lands in `_BSS`
(paragraph `0x0CE`) but the map shows `00A6:028A` — frame `0xA6` is DGROUP's
base, offset `0x28A` into the group.

## `.MAP` rendering (`map.rs`)

- **Uppercase** — TLINK upper-cases every public name in the listing
  (`_main` → `_MAIN`, `__C0argc` → `__C0ARGC`).
- **`Abs` tag** — absolute equates render `  Abs  ` in the 7-column gap after
  the address instead of blanks.
- **Publics by Value ordering** — absolutes group **first** (sorted by offset),
  then relocatable symbols by `(frame, offset)`. Ties break by **definition
  order** (the order PUBDEFs were seen), not alphabetically: `__ScanTodVector`
  (defined first) precedes `__RealCvtVector` at the shared `00A6:0284`.

## Library member placement (`lib.rs`)

The pulled members land in the output in **library order** (ascending member
index), independent of the order resolution discovered them. For this link the
pulled members are CS.LIB indices `2, 31, 35, 41, 51, 95, 96, 128, 143, 147,
159, 196, 198, 204, 258, 289` — strictly ascending — and that is the order
their `_TEXT`/`_DATA`/`_BSS` contributions concatenate. (Resolution still pulls
on demand to find the transitive set; only the final placement is re-sorted by
library index.)
