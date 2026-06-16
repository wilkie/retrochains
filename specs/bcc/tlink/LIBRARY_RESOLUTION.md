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

Per `../../formats/LIB_ARCHIVE.md`: a `0xF0` header record sets the page size
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

## Default-library COMENT (class `0x9F`)

TLINK also pulls libraries a translation unit *requests of its own accord*, via
a **default-library directive**: a `COMENT` record of class `0x9F` whose body is
a library name. The linker adds that name to the set it searches, after the
libraries named on the command line.

This is how a compiler ties its objects to its runtime without the user naming
the library. **MSC** stamps one into every object — `COMENT 88 … 9F "SLIBCE"`
for `/AS` (the small-model C runtime), `LLIBCE` for large, etc. (see
`../../MSC_FINGERPRINTS.md`). Real TLINK honours it: handed a lone MSC object it
parses the whole module — threads, COMDEF, and all — and then fails not on the
*format* but with

```
Fatal: Unable to open file 'slibce.lib'
```

i.e. it got far enough to act on the embedded directive and go looking for the
Microsoft runtime. So the OMF *format* is portable across the two toolchains;
what a complete MSC program needs is the MS C library (itself an OMF `.LIB`
TLINK could read), not a different object format.

**BCC 2.0 does not use this.** The compile-only objects we track (`MAIN.OBJ`,
`HELLO.OBJ`) carry only class `0x00` (`TC86 …`) and Borland's `0xE8`/`0xE9`/`0xEA`
debug/dependency COMENTs — no `0x9F`. Borland names the runtime explicitly on the
link line (`CS.LIB`), so this directive never fires for the BCC fixtures.

**What we implement.** `omf::parse` collects class-`0x9F` COMENT bodies into
`Module::default_libs` (and `omf::emit` re-emits them). When the command-line
libraries leave a symbol unresolved, [`resolve`](../../../crates/bcc-tlink/src/lib.rs)
loads each requested default library through a caller-supplied
`load_default_lib(name) -> Option<Vec<u8>>` and appends its members — so they're
searched *after* the command-line libraries, and a default library named by a
*pulled* member is honored too (the wanted set is recomputed each pass). The CLI
loader reads `<name>.LIB` from disk; `link_objects`/`link_image` pass a no-op
loader (the BCC pool names no default libraries, so nothing changes there). A
library that can't be loaded is simply skipped — the unresolved symbol then
surfaces with its name, rather than TLINK's eager `Unable to open …` fatal.

Verified by `synthetic_default_library_directive_pulls`: an object that names
`MYRT` and references a symbol only `MYRT.LIB` defines links *only* when the
loader supplies it, and the result is byte-identical to naming `MYRT.LIB` on the
command line. A real example is tracked at
`crates/bcc-tlink/tests/data/COMM_MSC.OBJ` (`0x9F` body `SLIBCE`).
