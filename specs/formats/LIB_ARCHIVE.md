# LIB — Microsoft/Borland Object Library Archive

Container format for collecting multiple `.OBJ` members into a single
file the linker can search by symbol. BCC 2.0's runtime is shipped
this way under `BC2/LIB/` (`CS.LIB`, `MATHS.LIB`, `GRAPHICS.LIB`,
`CWINS.LIB`, etc.).

Spec source: Microsoft library format documentation + empirical
inspection of BC2.zip's archives.

## File layout

```
[Library header at offset 0]
[padding to page boundary]
[Member 1 OBJ records...]
[padding to page boundary]
[Member 2 OBJ records...]
...
[padding]
[Dictionary records]
```

Each member is a complete OMF stream (THEADR ... MODEND) — see
[`OMF.md`](OMF.md) for the per-record layout. The library framing
adds three things on top: a header record at offset 0, page-aligned
member boundaries, and a dictionary at the end for symbol-to-member
lookup.

## Library header

```
byte 0:        0xF0 (library header tag)
bytes 1..2:    record length, LE — also defines the page size
                 (page_size = length + 3)
bytes 3..6:    dictionary file offset (LE u32)
bytes 7..8:    dictionary length in 512-byte blocks (LE u16)
byte 9:        flags (bit 0 = case-sensitive lookup)
byte 10..N:    padding to next page boundary, zero-filled
```

BC2.zip's libraries all use **page size 16** (length field = 0x000d).
Members are aligned to 16-byte boundaries; the inter-member padding
is zero bytes that the reader must skip.

## Page-aligned member layout

After each member's terminating MODEND, the file is padded with
zeros up to the next 16-byte boundary. The next member starts at
that boundary with its THEADR record.

A parser walks members like this:
1. Start at `page_size` (first member offset).
2. Skip any zero bytes.
3. If the next non-zero byte is `0x80` (THEADR), parse the member's
   OMF records until MODEND.
4. Round the post-MODEND offset up to the next page boundary.
5. Repeat until the file offset reaches the dictionary offset.

## Dictionary

A symbol → page-number index used by TLINK to find the member that
defines a given external symbol without scanning every OBJ. The
dictionary occupies `<dict_blocks>` 512-byte blocks at the end of
the file. Format details aren't critical for our use yet
(fingerprinting only needs to walk members); document them when we
need to build LIBs ourselves.

## TLIB strips BCC fingerprint COMENTs

Empirically (probe run 2026-05-13 against CS/MATHS/EMU/FP87/GRAPHICS/
OLDSTRMS/CWINS, ~810 members total), every BCC-compiled OBJ that
passes through TLIB loses these three COMENT records:

| Record                              | In BCC `-c` direct | In LIB members |
|-------------------------------------|---------------------|-----------------|
| COMENT class 0x00 (translator)      | yes (`TC86 Borland Turbo C++ 2.0`) | **stripped** |
| COMENT class 0xEA (model marker)    | yes (`01 09` for `-ms`) | **stripped** |
| COMENT class 0xE8 (trailer)         | yes (filename + mtime) | **stripped** |

In their place, TLIB inserts a single empty-payload COMENT class
0xA1 (`Default Library Search Name`) per member — this appears to
be TLIB's "this object was archived by Borland's library tool"
marker, not BCC's "this object was compiled by Borland's compiler"
fingerprint. The 0xA1 record carries no payload bytes other than
the flags + class bytes themselves.

This means the canonical TASM-2.51 fingerprint (translator string)
**cannot be used** to identify BCC-compiled code inside libraries.
You have to fall back on structural signals.

## Structural BCC fingerprint (translator-stripped)

Even without the three injected COMENTs, a BCC-compiled member is
still recognizable by the union of:

1. **LNAMES list**: single record containing exactly
   `["" "_TEXT" "CODE" "_DATA" "DATA" "_BSS" "BSS" "DGROUP"]`
   in that order (with the empty-string sentinel first).
2. **Per-segment SEGDEF ACBP bytes**: `0x28` for `_TEXT`, `0x48` for
   `_DATA` and `_BSS`.
3. **GRPDEF**: `DGROUP = {_DATA, _BSS}` in that segment-index order.
4. **Canonical record sequence**:
   `THEADR, [COMENT 0xA1,] LNAMES, SEGDEF×3, GRPDEF, [EXTDEF,] PUBDEF×N, LEDATA, [FIXUPP,] MODEND`
5. **Code idioms** in LEDATA:
   - `55 8B EC` — push bp / mov bp,sp prologue
   - `FF 76 NN` — push word ptr [bp+positive] (parameter forwarding)
   - `83 C4 NN` — add sp, imm8 (cdecl cleanup)
   - `5D C3` — pop bp / ret epilogue
6. **FIXUPP**: subrecords are fully explicit (no THREAD records),
   emitted in LIFO order relative to instruction encoding (see
   [`OMF.md`](OMF.md#fixupp-0x9c-16-bit-form)).

Hand-written ASM members built with TASM 2.x have a different
shape: typically a tiny initial LNAMES (just `""`), then multiple
small LNAMES records each preceding their SEGDEF, then a different
GRPDEF segment-index order. The math libraries in particular use
this MASM-style emission heavily.

## Per-library mix (BC2.zip 1991-04-23)

| Library      | Members | BCC-LNAMES-style | What it is           |
|--------------|---------|------------------|----------------------|
| CS.LIB       | 522     | 353 (68%)        | C runtime, small     |
| OLDSTRMS.LIB | 8       | 7  (88%)         | C++ streams, small   |
| CWINS.LIB    | 226     | 10  (4%)         | C/Windows, small     |
| MATHS.LIB    | 45      | 10  (22%)        | Math, small          |
| EMU.LIB      | 6       | 2  (33%)         | x87 emulator         |
| FP87.LIB     | 5       | 2  (40%)         | Real x87 stubs       |
| GRAPHICS.LIB | 4       | 0  (0%)          | Graphics primitives  |

A few takeaways:
- **CS.LIB and OLDSTRMS.LIB are mostly BCC-compiled C** — Borland
  built their C runtime in C.
- **MATHS / EMU / FP87 are mostly hand-written assembly** — typical
  for floating-point libraries of the era, where every cycle in the
  emulated FP path matters.
- **CWINS.LIB is overwhelmingly assembly** despite its name — the
  Windows interface layer is tight ASM stubs that thunk into the
  Windows API.
- **GRAPHICS.LIB is entirely assembly** — direct VGA/EGA register
  access is faster as ASM.

## Open questions

- The 0xA1 COMENT in BC2.zip's libraries always has empty payload.
  Microsoft's spec says class 0xA1 carries a length-prefixed library
  name. Does TLIB use the empty form as a Borland-specific marker,
  or is this a degenerate case of the standard format?
- Are there `BCC.EXE`-specific COMENTs anywhere in the library
  members? We've only checked classes 0x00/0xE8/0xE9/0xEA/0xA1 — a
  full histogram of every class in every library would surface
  unknowns.
- The library dictionary format isn't documented here. We'll need it
  when building libraries ourselves (eventual TLIB reimplementation).
