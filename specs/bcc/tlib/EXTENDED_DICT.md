# TLIB extended dictionary (`/E`) — partially reverse-engineered

The **extended dictionary** is an optional trailing structure TLIB writes after
the regular symbol dictionary when invoked with `/E`. It is a symbol→member
*dependency* index that lets TLINK pull a member's dependencies without a second
pass. It is **not** written by a plain `tlib lib +mod` (verified: our default
single/multi-member/multi-block libraries have no trailing structure and are
byte-exact). The Borland-shipped `LIB/*.LIB` archives *do* carry it.

The linker works without it, so it isn't needed to build the `.LIB` inputs the
linker fixtures use — it's a completeness / shipped-library-fingerprint goal.

## What's known

Captured from `tlib /E EX +M1 +M2` (2 members) and `FP87.LIB` (5 members):

- It begins immediately after the `nblocks`-block regular dictionary (so the
  file is `header + members + LIBEND + dict(nblocks·512) + extended_dict`).
- Layout of the 103-byte `EX` extended dict:
  ```
  ad 2b              magic (constant in both samples)
  02 00              member count (2; FP87 has 05 00 = 5)
  02 00 03 00 08 00 37 00   a small table (4 u16s — meaning TBD)
  ff ff … (12 bytes) 0xFF-initialised hash region
  00 05 "_TEXT" 05 "_DATA" 04 "_BSS" 06 "DGROUP" 04 "CODE" 04 "DATA" 03 "BSS" 00
                     a names list (segment/class names gathered from members)
  00 00 00 26 00 00 01 02 c0 01 32 00 …   per-symbol module-dependency records
  ```
- FP87's extended dict is 464 bytes with the same shape (magic, count 5, table,
  `0xFF` region, names list, dependency records).

## Still to do (for byte-exact `/E` / shipped libs)

- Decode the `ad 2b` header's table and the `0xFF` hash region (likely a
  symbol→member bucket table with its own hashing).
- Decode the per-symbol dependency records (`00 00 00 26 …`).
- Find and read TLIB's extended-dictionary writer (disassembly) to pin the
  exact field semantics and ordering, the way the regular dictionary was solved.
- Then: a `tool = "tlib"` `/E` fixture gating an extended-dict library, and a
  round-trip of a real `LIB/*.LIB` (e.g. `FP87.LIB`) byte-exact.

Until then, `crates/bcc-tlib` reproduces every *non-`/E`* library byte-exact.
