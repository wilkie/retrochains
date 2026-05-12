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
| 0x9c  | FIXUPP      | Relocations (not yet captured)           |
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

## Open questions

- The COMENT class 0xea data (`01 09` in fixture 002) decodes as
  what exactly? Likely `<model-id> <something>`. The 0x01 is
  plausibly small-model. The 0x09 is unexplained — possibly an
  options-flags byte. Capture a non-small-model `-c` fixture to
  see if 0x01 changes.
- FIXUPP records — what do they look like for an extern call?
  No fixture yet.
- TYPDEF records — does BCC emit any when the source uses a
  typedef?
