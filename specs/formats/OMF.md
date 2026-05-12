# OMF — Intel/Microsoft Object Module Format

The on-disk format `BCC -c` produces, `TASM` produces, and `TLINK`
consumes. Records are framed type-length-payload-checksum, in a
stream from start to end of the `.OBJ` file.

The spec source is the Intel TIS OMF document (1992) plus
Borland-specific extensions observed in our fixtures. Sections
below cite which one for each pattern.

## Record framing

Every record has the same wrapper:

```
byte 0:        record type
bytes 1..2:    payload+checksum length, little-endian
bytes 3..N-1:  payload
byte 3+len-1:  checksum
```

The **checksum** is chosen so the sum of every byte in the record
(type byte + length bytes + payload bytes + checksum byte) is
`0 mod 256`. Many OBJ producers emit `0` here as a "checksum not
present" sentinel, and most consumers (including TLINK) accept
either form. BCC always computes the real checksum.

The **length field includes the checksum byte**: a record with an
8-byte payload reports length 9.

## Record types BCC emits

Most types come in both 16-bit and 32-bit variants — the 32-bit
form has its type byte's low bit set. BCC under the small memory
model uses the 16-bit forms exclusively.

| Hex   | Name        | Purpose                                  |
|-------|-------------|------------------------------------------|
| 0x80  | THEADR      | Module header (module name)              |
| 0x88  | COMENT      | Comments — also Borland's debug records  |
| 0x8a  | MODEND      | End of module                            |
| 0x8c  | EXTDEF      | External symbol references               |
| 0x90  | PUBDEF      | Public symbol definitions                |
| 0x96  | LNAMES      | List of names referenced elsewhere       |
| 0x98  | SEGDEF      | Segment definition                       |
| 0x9a  | GRPDEF      | Group definition (e.g. DGROUP)           |
| 0x9c  | FIXUPP      | Relocations                              |
| 0xa0  | LEDATA      | Literal data block — actual code/data    |

## Per-record layouts

### THEADR (0x80)

```
<name-len-u8> <name-bytes>
```

BCC writes the lowercased source filename (e.g. `hello.c`).
Pascal-style length-prefixed string.

### COMENT (0x88)

```
<flags-u8> <class-u8> <class-specific-data>
```

`flags` is a bitfield: bit 7 = NoPurge (linker can't strip), bit
6 = NoList (don't show in listing). BCC writes `0x00` for both
of these in every observed COMENT.

`class` selects the sub-format. The classes BCC uses for `-c`
(observed in fixture 002):

| Class | Meaning (Borland)                | Data                                          |
|-------|----------------------------------|-----------------------------------------------|
| 0x00  | Translator (compiler id)         | length-prefixed compiler-version string       |
| 0xe8  | (Borland) trailing debug record  | `00 <name-len> <name> <packed-mtime-LE-u32>` |
| 0xe9  | (Borland) debug-info bookend     | `<packed-mtime-LE-u32> <name-len> <name>` (open) or empty (close) |
| 0xea  | (Borland) linker / model marker  | 2 bytes (`01 09` for small model in 002)      |

The class-0x00 record is the most distinctive fingerprint. The
data is the exact string `TC86 Borland Turbo C++ 2.0` length-
prefixed by its byte count (26 = 0x1a). Reading the bytes as
`00 1a 54 43 38 36 ...` it looks like class might be 0x1a — but
the 0x1a is the length prefix on the string; class is 0x00.

### LNAMES (0x96)

```
(<name-len-u8> <name-bytes>)*
```

A flat list of length-prefixed names referenced elsewhere by
1-based index. BCC's list for a minimal `-c` (fixture 002):

```
1: ""             (no-overlay sentinel)
2: "_TEXT"
3: "CODE"
4: "_DATA"
5: "DATA"
6: "_BSS"
7: "BSS"
8: "DGROUP"
```

The empty first entry is a BCC quirk — many compilers start at
index 1 with a real name.

### SEGDEF (0x98, 16-bit form)

```
<ACBP-u8> <length-LE-u16> <name-idx-u8> <class-idx-u8> <overlay-idx-u8>
```

The ACBP byte packs alignment / combine / big / proc:
- bits 7-5: alignment (000=abs, 001=byte, 010=word, 011=para, 100=page, 101=dword)
- bits 4-2: combine (000=private, 010=public, 100=stack, 101=common)
- bit 1: big (1 = segment length is exactly 64K)
- bit 0: proc (only relevant in 16/32 mixed mode)

BCC's observed ACBP values:
- `0x28` (= 0010 1000 → byte-align, public) for `_TEXT`
- `0x48` (= 0100 1000 → word-align, public) for `_DATA` and `_BSS`

The 16-bit length field is in bytes. The three indices are 1-based
into the LNAMES list.

### GRPDEF (0x9a)

```
<group-name-idx-u8> (0xFF <segment-idx-u8>)*
```

The `0xFF` byte before each segment index is the "segment-index
follows" marker. Other forms exist (group by external, group by
type) but BCC doesn't use them. The only group BCC defines is
`DGROUP = {_DATA, _BSS}`.

### PUBDEF (0x90, 16-bit form)

```
<base-group-idx-u8> <base-segment-idx-u8>
(<name-len-u8> <name-bytes> <offset-LE-u16> <type-idx-u8>)*
```

A PUBDEF can carry multiple public symbols sharing the same base.
BCC always emits one per record. `base-group-idx = 0` when the
symbol is relative to the segment directly (no group offset).
`type-idx = 0` means "untyped" — BCC doesn't emit TYPDEF records
in our fixtures.

### LEDATA (0xa0, 16-bit form)

```
<segment-idx-u8> <offset-LE-u16> <data-bytes>
```

The data bytes are placed into the named segment at the given
offset. For fixture 002's `int main(void) { return 0; }`, the
LEDATA payload is 9 bytes: `55 8B EC 33 C0 EB 00 5D C3`
(`push bp / mov bp, sp / xor ax, ax / jmp short +0 / pop bp /
ret`).

### EXTDEF (0x8c)

```
(<name-len-u8> <name-bytes> <type-idx-u8>)*
```

A flat list of external (unresolved) symbol references. Each entry
gets a 1-based EXTDEF index, used by FIXUPP target datums. BCC emits
`type-idx = 0` (untyped).

Fixture 108 (`call-printf`) emits a single `_printf` EXTDEF. Source
that calls multiple externs would emit them in source-encounter
order in one or more EXTDEF records.

### FIXUPP (0x9c, 16-bit form)

A stream of *subrecords*; the high bit of the first byte of each
subrecord selects the form:

- **THREAD subrecord** (bit 7 = 0): defines or redefines one of 4
  frame threads or 4 target threads, for compact reuse later. BCC
  doesn't use threads — every fixup is fully explicit. This is a
  fingerprint of BCC vs. TASM 3.x and MSVC.
- **FIXUP subrecord** (bit 7 = 1): one fixup.

#### FIXUP subrecord layout

```
Locat (2 bytes, the first holds the high bits):
  byte 0:  1 M L L L L D D     bit 7 = 1 (fixup marker)
                              bit 6 = M  (1 = segment-relative, 0 = self-relative)
                              bits 5-2 = Location (4-bit type code)
                              bits 1-0 = high 2 bits of data-record offset
  byte 1:  D D D D D D D D     low 8 bits of data-record offset

Fix Data byte:
  bit 7: F  (1 = use frame thread, 0 = explicit frame method)
  bits 6-4: Frame (frame method 0-7, or thread index if F=1)
  bit 3: T  (1 = use target thread, 0 = explicit target method)
  bit 2: P  (concatenated with bits 1-0 = 3-bit target method)
  bits 1-0: Target (low 2 bits of target method)

Then (in order, omit each if the method takes no datum):
  Frame Datum   (1 byte index, for frame methods 0/1/2 only)
  Target Datum  (1 byte index, for target methods 0/1/2/4/5/6)
  Target Displacement (16-bit LE, only if P=0 → target methods 0-2)
```

The 10-bit "data-record offset" is the byte position *within the
preceding LEDATA's data payload* where the fixup is to be applied.
Offsets are 0-based from the first data byte (i.e. they exclude
LEDATA's 3-byte `<seg-idx> <offset-LE-u16>` header).

#### Frame methods (TIS, 3-bit)

| Code | Mnemonic | Datum         | Meaning                                  |
|------|----------|---------------|------------------------------------------|
| F0   | SEGDEF   | SEGDEF idx    | Frame is the named segment               |
| F1   | GRPDEF   | GRPDEF idx    | Frame is the named group                 |
| F2   | EXTDEF   | EXTDEF idx    | Frame is whatever frame the extern lives in |
| F3   | —        | —             | Reserved                                 |
| F4   | LOCATION | —             | Frame is the segment of the data record being fixed |
| F5   | TARGET   | —             | Frame is the segment of the target       |
| F6   | NONE     | —             | Absolute (no frame)                      |
| F7   | —        | —             | Reserved                                 |

#### Target methods (3-bit, P||TargetLow)

| Code | Mnemonic | Datum         | Displacement | Meaning                          |
|------|----------|---------------|--------------|----------------------------------|
| T0   | SEGDEFᴰ  | SEGDEF idx    | yes          | Offset from named segment        |
| T1   | GRPDEFᴰ  | GRPDEF idx    | yes          | Offset from named group          |
| T2   | EXTDEFᴰ  | EXTDEF idx    | yes          | Offset from external             |
| T3   | —        | —             | —            | Reserved                         |
| T4   | SEGDEF   | SEGDEF idx    | no           | Named segment, displacement in LEDATA |
| T5   | GRPDEF   | GRPDEF idx    | no           | Named group, displacement in LEDATA   |
| T6   | EXTDEF   | EXTDEF idx    | no           | External, displacement in LEDATA      |
| T7   | —        | —             | —            | Reserved                         |

The "no displacement" methods (T4–T6) save 2 bytes per fixup by
relying on the LEDATA payload to already contain the target's
in-segment offset. BCC uses T4 and T6 exclusively in our fixtures —
never the explicit-displacement T0–T2 forms. Another BCC fingerprint.

#### Location types (4-bit)

| Code | Width | Meaning                                          |
|------|-------|--------------------------------------------------|
| 0    | 8     | Low byte of offset                               |
| 1    | 16    | 16-bit near offset (most common — BCC always uses this) |
| 2    | 16    | Base / segment selector                          |
| 3    | 32    | Far pointer (segment:offset)                     |
| 4    | 8     | High byte of offset                              |
| 5    | 16    | Loader-resolved 16-bit offset                    |

BCC small-model only ever emits location type 1 (near offset). The
other types will appear as we capture large/huge model fixtures.

#### BCC's four observed fixup recipes (-ms model)

Each row shows the FIXUP subrecord bytes BCC emits, organized by
the kind of reference being patched.

| Reference                          | M | Loc | F  | T  | Frame datum | Target datum | Bytes | Hex example       |
|------------------------------------|---|-----|----|----|-------------|--------------|-------|-------------------|
| Near call to extern (rel16)         | 0 | 1   | F5 | T6 | (none)      | EXTDEF idx   | 4     | `84 0c 56 01`     |
| Load offset of string in `_DATA`    | 1 | 1   | F1 | T4 | GRPDEF idx  | SEGDEF idx   | 5     | `c4 08 14 01 02`  |
| Load offset of var in `_BSS`        | 1 | 1   | F1 | T4 | GRPDEF idx  | SEGDEF idx   | 5     | `c4 04 14 01 03`  |
| Load offset of fn in `_TEXT`        | 1 | 1   | F5 | T4 | (none)      | SEGDEF idx   | 4     | `c4 12 54 01`     |

Reading the hex column: `84` (FIXUP, self-rel) / `c4` (FIXUP, seg-rel)
sit in the Locat high byte alongside the high bits of the data offset;
the second byte is the offset low byte. Then Fix Data (`56` / `14` / `54`),
then the datums in F-then-T order.

**Pattern:** data references go through DGROUP (frame F1 + target
`_DATA`/`_BSS` via T4) because the small-model image physically merges
those two segments; code references use F5 ("target's own frame") because
`_TEXT` has no enclosing group. Self-relative calls reuse F5 since the
target segment *is* the frame.

#### When BCC omits a fixup entirely

A near-call to a sibling function in the same TU does **not** produce a
FIXUPP — BCC pre-computes the `rel16` displacement at compile time and
embeds it directly in the LEDATA. Fixture 107 (`call-sibling-obj`) is
the canonical example: `main` calls `f`, both live in one LEDATA, and
the linker sees zero relocations for that call.

This is structurally important: TLINK gets handed a self-contained code
blob whenever possible, and FIXUPP entries only appear when the
displacement genuinely can't be known until link time (cross-TU calls,
externs, or any reference into `_DATA`/`_BSS`/`_TEXT`-by-symbol-name).

### MODEND (0x8a, 16-bit form)

```
<flags-u8> [<frame-fixupp> <target-fixupp> <offset-LE-u16>]?
```

The flags byte indicates whether a start address follows: bit 7
(main module), bit 6 (start address present), bit 0 (logical
start). BCC's `main` function isn't marked as the entry point in
the OBJ — the linker picks `_main` up via the PUBDEF, not via
MODEND's start-address slot. So BCC always emits the minimal
`<flags=0x00>` form (1 byte payload + 1 byte checksum).

## Record order in a minimal `-c` OBJ

Observed in fixture 002:

1. THEADR — module name
2. COMENT class 0x00 — compiler identifier
3. COMENT class 0xe9 — debug-info open (with timestamp + filename)
4. COMENT class 0xe9 — debug-info close (no extra data)
5. COMENT class 0xea — memory-model / linker directive
6. LNAMES — name list
7. SEGDEF — `_TEXT`
8. SEGDEF — `_DATA`
9. SEGDEF — `_BSS`
10. GRPDEF — `DGROUP`
11. PUBDEF — `_main`
12. LEDATA — function bytes
13. COMENT class 0xe8 — trailing debug record (timestamp + filename)
14. MODEND — no entry point

Total: 214 bytes for the smallest possible OBJ (empty `main`).

When the program uses externs, globals, or function pointers, two
extra record types appear at predictable points:

- **EXTDEF** — emitted after the GRPDEF, before any PUBDEF. (Fixture
  108: EXTDEF for `_printf` appears at slot 10, between GRPDEF and
  the PUBDEF for `_main`.)
- **FIXUPP** — emitted immediately after the LEDATA whose bytes it
  patches. One FIXUPP can hold multiple FIXUP subrecords if they
  all apply to the same preceding LEDATA. (Fixture 108: a single
  FIXUPP carries both the `_printf` and the string-pointer fixups.)

When the program defines an uninitialized global, `_BSS`'s SEGDEF
length grows past 0 and the global gets its own PUBDEF whose
`base-segment-idx` points at `_BSS` (segment 3 in BCC's standard
LNAMES layout). Fixture 109: `_x` is published in `_BSS` with offset 0,
and the `_BSS` SEGDEF length goes from `00 00` to `02 00`.

## Open questions

- The COMENT class 0xea data (`01 09` in fixture 002) decodes as
  what exactly? Likely `<model-id> <something>`. The 0x01 is
  plausibly small-model. The 0x09 is unexplained — possibly an
  options-flags byte. Capture a non-small-model `-c` fixture to
  see if 0x01 changes.
- Does BCC ever emit a THREAD subrecord in FIXUPP, or always
  explicit FIXUPs? So far (fixtures 002, 107-110) it's always
  explicit. A larger TU with many cross-references would tell us
  whether there's a threshold or it's truly never.
- LEDATA size limit: the spec says LEDATA data can be up to 1024
  bytes (16-bit form). What does BCC do when a function exceeds
  that? Multiple LEDATAs with consecutive offsets, presumably.
  No fixture yet.
- LIDATA (0xa2) — iterated data block, for repeated patterns
  (e.g. `char buf[100] = {0};`). BCC may use this for large
  zero-initialized data instead of a giant LEDATA. No fixture
  yet.
- TYPDEF records — does BCC emit any when the source uses a
  typedef?
