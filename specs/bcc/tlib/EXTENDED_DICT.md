# TLIB extended dictionary (`/E`) — structure decoded, not yet implemented

The **extended dictionary** is an optional trailing structure TLIB writes after
the regular symbol dictionary when invoked with `/E`. It is a compressed,
self-contained catalog of every member's defining segments plus a symbol→member
hash, letting TLINK resolve dependencies without re-scanning members. It is
**not** written by a plain `tlib lib +mod` (our default single/multi-member/
multi-block libraries have no trailing structure and are byte-exact). The
Borland-shipped `LIB/*.LIB` archives *do* carry it.

The linker works without it, so it isn't needed to build the `.LIB` inputs the
linker fixtures use — it's a completeness / shipped-library-fingerprint goal.

## Where it's written

TLIB.EXE writer at file offset **`0x2ee0`** (magic load `mov ax,0x2bad` at
`0x2f0f`). The extended dict begins immediately after the `nblocks`-block regular
dictionary, so the file is `header + members + LIBEND + dict(nblocks·512) +
extended_dict`. `0x28f7` = "write u16 to file".

## Structure (from disassembly + the `EX` 2-member and `FP87` 5-member samples)

1. **Header** — `0x2f0f`..`0x2f2e`:
   - `2bad` magic word.
   - `[0x5df]` = member count (`EX`=2, `FP87`=5).
   - `[0x5e5]`, `[0x5e7]` (two more words; `EX` = `0002`, `0003`).
2. **Bucket table + names list** — `call 0x2916`:
   - writes `[0x5f1]` (`EX`=8 = bucket count) and `[0x5f3]` (`EX`=0x37).
   - writes `[0x5f1]` words of `0xFFFF` — the empty symbol→member hash buckets
     (`EX`: 8 words = 16 `0xFF` bytes).
   - walks name list `[0x5e9]`, writing each entry **length-prefixed** via
     `0x28a3` — the union of member LNAMES (`EX`:
     `"" _TEXT _DATA _BSS DGROUP CODE DATA BSS`).
3. **Symbol hash build** — `call 0x2975`: walks the public-symbol list `[0x5f5]`,
   hashes each name (`0x3328`), inserts/looks-up in the bucket table (`0x33aa`),
   assigning a per-symbol index (the `neg si` / store at node+4 logic).
4. **Per-member compacted descriptors** — main loop `0x2f35`..`0x2f5e` over list
   `[0x5d7]`, `call 0x2d2b` per node: writes a word (`node+0x1e`→`+4`), a value
   (`node+0x18` via `0x27e5`), then packs the member's segment/group/class **name
   indices** (`node+0x14/+0x16/+0x1a/+0x1c`, each clamped to 6/0xe/2/6 → 7/0xf/3/7)
   into a bitfield (`<<12 | <<8 | <<1`). A compressed per-member segment map.

`EX` tail bytes confirming the shape:
```
ad 2b 02 00 02 00 03 00      header (magic, count=2, 0002, 0003)
08 00 37 00                  bucket_count=8, [0x5f3]=0x37
ff×16                        8 empty buckets
00 05 "_TEXT" 05 "_DATA" 04 "_BSS" 06 "DGROUP" 04 "CODE" 04 "DATA" 03 "BSS" 00
00 00 00 26 00 00 01 02 c0 01 32 00 …    per-member descriptors + symbol records
```

## Decoded format (from controlled E1/E2/E3 probes + disassembly)

**Header** (6 words): `2bad`, `member_count`, `total_segment_count`,
`total_public_count`, `name_count`, `2·name_count + Σ(1+len(name))`.

**Bucket table**: `name_count` words of `0xFFFF` — always empty (TLIB never
populates it; the linker does at load).

**Names list**: a fixed seed `["", _TEXT, _DATA, _BSS, DGROUP, CODE, DATA, BSS]`
(indices 0–7) followed by each member's unique segment/class names not already
present, each length-prefixed, then a `0x00` terminator. `name_count` counts the
seed + the added names (E1: 8 seed + `CSEG` = 9).

**Descriptors** (per member, in add order), 6-byte records referencing **regular
dictionary entry offsets**:
- module record: `00 00 <modoff:u16> 00 00` — `modoff` = the `name!` entry's
  offset in the regular dict block.
- segment record: `<page:u8> <npubs:u8> 00 <segNameIdx:u8> <packed:u16>` —
  `segNameIdx` indexes the names list; `packed` encodes the segment's
  align/combine/class (the `0x2d2b` bitfield).
- public records (`npubs` of them): `<dictoff:u16> 00 <page:u8> 00 00` —
  `dictoff` = the public symbol's entry offset in the regular dict.

Verified against E3 (1 member, 1 `CSEG` segment, 3 publics `P1/P2/P3`): module
`C!`@0x26, publics at 0x2c/0x32/0x38 — exactly the ext-dict references.

## Still to do (to implement byte-exact `/E`)

- Decode the **`packed:u16` segment word** (`0x2d2b` bitfield: `seg<<12 |
  grp<<8 | class<<1 | …`, fields clamped to 6/0xe/2/6) for arbitrary segments,
  and confirm **multi-segment** members emit one segment record each.
- Implement the emitter (all fields above are computable from the parsed members
  + the regular-dict offsets the writer already produces); wire `/E` through
  `main.rs`.
- Fixtures: a `tool = "tlib"` `/E` fixture, then a byte-exact round-trip of a
  shipped `LIB/*.LIB` (`FP87.LIB`, 5072 B, smallest).

Scope: comparable to the whole regular-dictionary effort. Until done,
`crates/bcc-tlib` reproduces every *non-`/E`* library byte-exact (4263–4266).
