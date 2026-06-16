# TLINK MZ executable output

How Turbo Link 4.0 lays out the DOS MZ (".EXE") header and load image.
Reverse-engineered byte-exact against the standalone-linker fixtures
`fixtures/c/linking/standalone/` (4258 single OBJ, 4259 two OBJs + cross-module
call) and reproduced by `crates/bcc-tlink` (`mz.rs`, `link.rs`).

## Header layout

TLINK writes a **512-byte header** (0x20 paragraphs) for small images, then the
load image. The first 28 bytes are the standard MZ header:

| Off | Field | Value (observed) | Notes |
|----|-------|------------------|-------|
| 0x00 | `e_magic` | `4D 5A` (`MZ`) | |
| 0x02 | `e_cblp` | `file_size % 512` | bytes on last 512-byte page |
| 0x04 | `e_cp` | `ceil(file_size/512)` | page count |
| 0x06 | `e_crlc` | reloc count | 0 for self-contained images |
| 0x08 | `e_cparhdr` | `0x0020` | header size in paragraphs (= 512 bytes) |
| 0x0a | `e_minalloc` | `ceil((mem_size − file_image)/16)` | extra paragraphs (BSS+stack) |
| 0x0c | `e_maxalloc` | `FFFF` | |
| 0x0e | `e_ss` | stack segment paragraph | relative to load |
| 0x10 | `e_sp` | stack length | e.g. `0x0100` for `.STACK 100h` |
| 0x12 | `e_csum` | `0000` | **TLINK leaves the checksum zero** (a fingerprint) |
| 0x14 | `e_ip` | entry offset | from MODEND start address |
| 0x16 | `e_cs` | entry segment paragraph | relative to load |
| 0x18 | `e_lfarlc` | `0x003E` | relocation-table offset (see below) |
| 0x1a | `e_ovno` | `0000` | overlay number |

### The 0x1c signature

At offset **0x1c** TLINK writes a fixed 6-byte blob:

```
01 00 fb 30 6a 72
```

It is **byte-identical across distinct links** (4258's 6-byte image and 4259's
12-byte image both carry it unchanged), so it is a linker version/identity
signature, not image-derived data. Its exact meaning is still unexplained — it
is not the faketime DOS timestamp (that is `0x6000`/`0x1697`, which appears in
the *OBJ*'s `0xe9` COMENT, not here). Reproduced verbatim as `TLINK_SIGNATURE`.

### Relocation table & header padding

`e_lfarlc = 0x3e`: TLINK leaves a gap after the signature and starts the
relocation table at **0x3e** (MS LINK packs it right after the header at 0x1e —
see `../../linkers/DIFFERENCES.md`). Each relocation is a 4-byte `offset:segment`
pair, where `segment` is the location's **frame paragraph** and `offset` is its
distance into that frame. The header is then padded with zeros up to 512 bytes.

4258/4259 have **zero relocations** (near self-relative calls resolve fully at
link time). **4260** (`tlink-far-call`) is the first with one: a far `CALL` to an
`EXTRN FAR` proc deposits a load-relative segment word, so TLINK writes
`e_crlc = 1` and one table entry `offset=0x0003 segment=0x0000` — the `9A` far
call's segment word, at `_TEXT` offset 3 in frame 0. DOS adds the load segment to
that word at load time.

> Open: whether the 512-byte header is a hard minimum or `0x3e + reloc_table`
> rounded up to a paragraph once relocations exceed ~0x1c2 entries. Needs a
> fixture with many relocations to pin.

## Load image & layout

The load image is the combined segments concatenated, each combined segment
started on a **paragraph boundary** (so CS/SS/group bases are exact paragraph
numbers). Within a combined segment, each contributing module segment is placed
at its own SEGDEF alignment (word for TASM `.CODE`, para for `.STACK`).

- Segment **order** is first-appearance across the input objects (`_TEXT`,
  `_DATA`, `STACK` for these fixtures).
- The **file image** runs from offset 0 to the end of the last *initialized*
  segment; trailing uninitialized segments (BSS, STACK) contribute only to
  `mem_size` (hence `e_minalloc`), not to file bytes. 4258: `_TEXT` = 6 bytes →
  518-byte file. 4259: combined `_TEXT` = MAIN(7) + 1 pad + SUB(4) = 12 bytes.
  When an *initialized* segment follows the stack (4260's `FARSEG` after
  `STACK`), the stack region falls *inside* the file image as zero fill, so
  `mem_size == file_image` and `e_minalloc` drops to 0.
- **Entry point** comes from a module's MODEND start address (TASM `END START`),
  resolved through segment combination to `CS:IP`. (BCC programs instead entry
  via the C startup's `_main` PUBDEF; not yet exercised standalone.)
- **Stack** is the segment with combine = stack; `e_ss` = its paragraph,
  `e_sp` = its length.

## Fixups applied at link time

Handled so far (`link.rs::apply_fixup`), location type 1 (near 16-bit offset):

- **Self-relative** (M=0), e.g. a near `CALL` to an `EXTRN` (frame F5, target T6
  = EXTDEF): patched value = `target − (patch_addr + 2)`, added to the
  displacement already in the LEDATA. 4259's `CALL ANSWER` → `E8 05 00`.
- **Segment-relative** (M=1), target T4 (SEGDEF) / via group frame F1: value =
  `target − frame_base`. (Exercised by data-pointer loads; BCC's recipes in
  `specs/formats/OMF.md`.)
- **Far pointer** (location type 3), e.g. a far `CALL` to an `EXTRN FAR` (M=1,
  frame F5, target T6): the 4-byte field gets `offset = target's distance into
  its frame` then `segment = frame paragraph` (load-relative), and the segment
  word gets a **runtime relocation** entry. 4260's `CALL FARPROC` →
  `9A 00 00 11 00` + reloc `(0x0003, 0x0000)`.
- **Segment selector** (location type 2), e.g. `MOV AX, @DATA` (M=1, frame F5,
  target T5 = GRPDEF): deposit the **frame paragraph** into the 16-bit immediate
  and relocate it. 4261's `MOV AX, @DATA` → `B8 01 00` (DGROUP base = para 1) +
  reloc `(0x0001, 0x0000)`.

All fixups share one **frame paragraph** rule, by frame method: F1 = a named
group's base paragraph (lowest load paragraph among its segments), F4 = the
patched location's own segment, F5 = the target's frame (a segment's paragraph,
or a group target's base). Near fixups produce **no relocation** (value final
once placed); segment selectors and far pointers each add one `e_crlc` entry.
Still ahead: far *data* pointers, then library (`.LIB`) resolution and linking
BCC-compiled OBJs (C startup + DGROUP) standalone.
