# MSC OMF dialect

This is the OMF record layout `CL /c /AS` produces, based on the
Phase 1 corpus (fixtures 4075–4125). It is **not** the OMF spec — it's
MSC's particular dialect: which records, in which order, with which
field choices. We discovered each rule the hard way; the matching
memory note (in `~/.claude/.../memory/`) carries the empirical reasoning
and the fixture that pinned it.

## Record order (top-to-bottom)

```
THEADR                                  source filename, uppercased
COMENT class=00 "MS C"                  translator id
COMENT class=9F "SLIBCE"                default library
COMENT class=9D "..."                   MS-Pascal style (purpose unclear)
COMENT class=A1 "CV7"                   CodeView hint (we leave it empty)
LNAMES                                  ["", DGROUP, _TEXT, CODE,
                                         _DATA, DATA, CONST, _BSS, BSS]
SEGDEF _TEXT                            length = text_bytes
SEGDEF _DATA                            length = data_bytes
SEGDEF CONST                            length = const_bytes
SEGDEF _BSS                             length = 0
GRPDEF DGROUP                           members CONST, _BSS, _DATA
FIXUPP THREADs                          (see below)
EXTDEF                                  (possibly split, see COMDEF)
COMDEF                                  if any tentative globals
EXTDEF (continued)                      function names, post-COMDEF
PUBDEFs                                 one record per source-order
                                         (group,seg) bucket transition
COMENT class=A2                         link-pass marker
LEDATA CONST (one per string)           with 2-byte alignment between
LEDATA _DATA                            init globals concatenated
FIXUPP                                  for _DATA's slot fixups
LEDATA _TEXT                            functions concatenated
FIXUPP                                  for _TEXT's instruction fixups
MODEND                                  no-entry, type 0x02
```

See [`msc-pubdef-source-order`](../../~/.claude/.../msc_pubdef_source_order.md)
for the PUBDEF ordering rule; [`msc-ledata-layering`](../../~/.claude/.../msc_ledata_layering.md)
for the LEDATA/FIXUPP interleave.

## FIXUPP THREADs

MSC pre-registers four target threads and two frame threads at the
top of every OBJ. Subsequent FIXUP subrecords reference them by a
single byte instead of carrying explicit method+index pairs.

```
T0 → SEGDEF 3 (CONST)
T1 → SEGDEF 2 (_DATA)
T2 → SEGDEF 1 (_TEXT)
T3 → SEGDEF 4 (_BSS)
F0 → SEGDEF 1 (_TEXT) — method T0=SEGDEF
F1 → GRPDEF 1 (DGROUP) — method F1=GRPDEF
```

The four 2-byte THREAD payloads (`00 03`, `01 02`, `02 01`, `03 04`)
followed by the two 2-byte frame payloads (`40 01`, `45 01`) form the
13-byte FIXUPP record `9c 0d 00 ...` that every MSC OBJ begins with
after the SEGDEFs.

## FIXUP shape menu

| Shape           | Frame thread + target            | Use case                          |
|-----------------|----------------------------------|-----------------------------------|
| `c4 off 9c`     | DGROUP / CONST                   | Load string-literal address       |
| `c4 off 9d`     | DGROUP / _DATA                   | PUBDEF-init global access         |
| `c4 off 56 idx` | target's frame / explicit EXTDEF | COMDEF or pointer-to-COMDEF       |
| `84 off 56 idx` | self-rel, explicit EXTDEF        | External call                     |

See [`msc-fixup-shapes`](../../~/.claude/.../msc_fixup_shapes.md) for
the placeholder-byte semantics of each.

The frame methods MSC emits are **F0** (segment), **F1** (group, for
DGROUP-relative data) and **F5** (the target's own frame, for extern
references — the `… 56 …` shapes). It never emits **F2** (extern-named
frame). `bcc-tlink`'s `apply_fixup` resolves all three byte-exact: F5
falls to `target.frame_para` (the group base for a grouped target, the
segment paragraph otherwise), so a grouped extern reference framed F5
deposits the same DGROUP-relative offset as the equivalent F1 form —
verified install-free by `synthetic_msc_f5_frame` in
`crates/bcc-tlink/tests/synthetic.rs`.

## CONST layout

One LEDATA per string, each at a 2-byte-aligned offset. Odd-length
strings leave a 1-byte gap zero-filled by the linker. The placeholder
inside `_DATA` for a pointer-to-string carries the string's CONST
offset; the FIXUP is the no-displacement form. See
[`msc-const-alignment`](../../~/.claude/.../msc_const_alignment.md).

## COMDEF length encoding

A COMDEF entry is `name_len, name, type_idx=0, data_type=0x62 (NEAR),
length`. The length field is:

- `≤ 0x80` → single byte = length.
- `0x81..=0xFFFF` → byte `0x81` then 2-byte LE u16.

See [`msc-comdef-emission`](../../~/.claude/.../msc_comdef_emission.md)
for the EXTDEF split MSC does around the COMDEF.
