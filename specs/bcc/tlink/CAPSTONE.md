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
- **Alignment — pack within a group, paragraph between frames.** A segment that
  continues the *same group* as the immediately-preceding segment is placed at
  its own SEGDEF alignment (byte/word/para), packing tight — DGROUP's interior
  segments butt against each other (`_DATA` ends at `0xCE3`, `_CVTSEG` starts at
  `0xCE4`, *not* `0xCF0`). Every other segment — the first member of a group, or
  any ungrouped segment — starts on a fresh paragraph so it owns a clean frame.
  (The single-DGROUP-member standalone fixture 4261 depends on this: its
  `_DATA` paragraph-aligns because it is the first DGROUP member.)

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
