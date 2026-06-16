# TLINK library (`.LIB`) resolution

How TLINK pulls object modules out of `.LIB` archives to satisfy unresolved
externals. Reverse-engineered against `fixtures/c/linking/standalone/4262`
(byte-exact) and reproduced by `crates/bcc-tlink` (`archive.rs`, `lib.rs`).

## Command line

Libraries are the **fourth** comma field of the TLINK command line —
`tlink objfiles, exefile, mapfile, libfiles` — `+`-joined, default extension
`.LIB`. 4262 links `MAIN.OBJ` against `MYLIB.LIB`:

```
tlink /m MAIN.OBJ,MAIN.EXE,MAIN.MAP,MYLIB.LIB
```

`MAIN.OBJ` references `ADDONE`, which is **not** named on the link line; TLINK
finds it in a member of `MYLIB.LIB` and pulls that member in.

## Selection rule (what we implement)

Named object files always link. Then, repeatedly:

1. Compute the set of **unresolved** externals = (every module's EXTDEFs) −
   (every included module's PUBDEFs).
2. Find the first library member that defines one of them; include it.
3. Repeat. A pulled member's own EXTDEFs join the unresolved set, so members are
   pulled **transitively** (A needs B, B needs C → all three link). Stop when a
   pass pulls nothing.

Pulled members are appended after the named objects in pull order, so their
segments combine after the explicit objects' — in 4262, `ADD`'s `_TEXT` lands at
offset `0x0A`, right after `MAIN`'s 10 bytes, and the `CALL ADDONE` resolves
there (`B8 05 00 E8 04 00 …` + `40 C3` = `INC AX / RET` at `0x0A`).

A member that defines no currently-needed symbol is **not** pulled — selective
linking, the whole point of an archive.

## `.LIB` framing we parse

Per `../formats/LIB_ARCHIVE.md`: a `0xF0` header record sets the page size
(`length field + 3`, =16 for BC2/TLIB libraries) and records the dictionary file
offset. Members are complete OMF streams (`THEADR … MODEND`) on page boundaries,
zero-padded between. We walk members from `page_size` to the dictionary offset,
skipping padding, parsing each member as a module.

We **skip the dictionary** (the symbol→page hash table TLINK uses to avoid a
linear scan): the linker builds its own symbol→member map from each member's
PUBDEFs, so a linear walk is enough and avoids depending on the dictionary's
exact hashing. (The dictionary remains relevant if we ever reimplement TLIB.)

> The library inputs are built by the real **TLIB.EXE** (now shipped in the BC2
> oracle's `BIN/` — `tlib MYLIB +ADD`) and tracked alongside the `.ASM`
> provenance, like the standalone OBJ inputs.
