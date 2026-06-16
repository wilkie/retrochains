# TLINK overlays (VROOMM)

Borland's overlay system — VROOMM (Virtual Run-time Object-Oriented Memory
Manager). Overlaid module code lives in an **overlay area** appended to the EXE
and is swapped into a resident buffer on demand by an `INT 3F` overlay manager.
This is a large, still-unimplemented feature in `crates/bcc-tlink`; this file
records what's reverse-engineered so far. Reproduction reference:
`MAIN.C` (calls `square`) + `MOD.C` (`square`), medium model.

## Invocation (cracked)

Overlays need a far model (medium/large/huge) and the overlay manager from
`OVERLAY.LIB`. Selection of which modules to overlay is **positional `/o`** on
the TLINK object list, *not* parentheses:

```
C0M.OBJ+MAIN.OBJ /o MOD.OBJ, PROG.EXE, PROG.MAP, CM.LIB+OVERLAY.LIB
```

Modules **after** `/o` are overlaid; everything before stays resident. (The
parenthesis syntax some TLINK docs show — `(MOD.OBJ)` — is *not* accepted by
TLINK 4.0; it errors `Unable to open file '(mod.obj)'`. A leading `/o` before
all modules overlays everything including the entry → `Program entry point may
not reside in an overlay`.) Per-module, the compiler marks overlaid code:
`bcc -Y` makes a module overlay-aware (resident), `bcc -Yo` overlays it. Driving
the command-line TLINK needs a response file (`@RESP`) because DOSBox's shell
splits the spaces around `/o`.

## Resident image layout

From the reference `.MAP` (load order), the resident image is:

| Segment | Class | Role |
|---|---|---|
| `_TEXT` | CODE | C0 startup |
| `MAIN_TEXT` | CODE | resident module code |
| `_OVRTEXT_` | CODE | the overlay manager runtime (`~0x943` bytes, from OVERLAY.LIB) |
| `_OVERLAY_` `_OVRDATA_` `_STUB_` `_EXTSEG_` `_EMSSEG_` `_VDISKSEG_` `_EXEINFO_` | OVRINFO | overlay bookkeeping (tables below) |
| `_1STUB_`, `MOD_TEXT` | STUBSEG | the **stub** for each overlaid module |
| `_DATA` … `_STACK` | DATA/BSS/STACK | normal DGROUP |

Note `MOD_TEXT` appears **twice**: once here in class `STUBSEG` (the resident
stub) and once in the overlay area in class `:OVY` (the real code). The stub is
a small thunk — `CD 3F` (`INT 3F`) plus an overlay descriptor — that the
manager patches/uses to fault the real segment in. Calls from resident code to
an overlaid symbol resolve to the **stub**, not the real segment.

## Overlay area (`FBOV`)

After the resident load image, TLINK appends the overlay area. Each overlaid
segment gets an `FBOV` record:

```
'F' 'B' 'O' 'V'   (46 42 4F 56)   magic
<u32>             flags/header size  (observed 0x20)
<u32>             file offset of the overlay data  (0x1c90)
<u32>             overlaid code size  (0x19)
<code bytes…>     the overlaid segment image
```

In the reference, the `FBOV` record sits at file `0x2150`, immediately followed
by `square`'s code (`55 8B EC 56 8B 76 06 8B C6 F7 EE … CB` = `push bp; mov
bp,sp; push si; mov si,[bp+6]; mov ax,si; imul si; …; retf`). The resident stub
contains `CD 3F` (the `INT 3F` to the manager).

## What a byte-exact implementation needs

No partial milestone is byte-exact — these interlock:

1. Parse positional `/o` and tag modules overlaid.
2. Pull the overlay manager (`_OVRTEXT_` + its OVRINFO data) from `OVERLAY.LIB`.
3. Emit a `STUBSEG` stub per overlaid module (`INT 3F` + descriptor); resolve
   resident references to the stub.
4. Build the OVRINFO tables — `_OVRDATA_` (per-overlay state), `_EXEINFO_`
   (`0xD8` bytes: overlay count, sizes, file offsets), `_STUB_`.
5. Move overlaid segments out of the resident image into the `FBOV` overlay
   area; keep their own fixups overlay-load-relative.
6. MZ header + relocations spanning resident + overlay framing.

This is comparable in size to the rest of the linker; track it as its own
effort. Reference inputs are tracked under `crates/bcc-tlink/tests/data/overlay/`.
