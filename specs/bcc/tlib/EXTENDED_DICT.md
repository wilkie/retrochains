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

## Still to do (to implement byte-exact `/E`)

- Decode `0x2975`/`0x33aa` (the symbol-hash insertion → bucket-table contents and
  the per-symbol records) and `0x3328` (the extended-dict symbol hash).
- Decode `0x2d2b`'s descriptor packing fully and the node fields it reads
  (`+0x14`…`+0x1e`) — i.e. reconstruct the in-memory member representation.
- Pin the header words `[0x5e5]/[0x5e7]/[0x5f3]` and the bucket count `[0x5f1]`
  (computed before the write; find where they're set).
- Implement the emitter; wire `/E` through `main.rs`.
- Fixtures: a `tool = "tlib"` `/E` fixture, then a byte-exact round-trip of a
  shipped `LIB/*.LIB` (`FP87.LIB`, 5072 B, smallest).

Scope: comparable to the whole regular-dictionary effort. Until done,
`crates/bcc-tlib` reproduces every *non-`/E`* library byte-exact (4263–4266).
