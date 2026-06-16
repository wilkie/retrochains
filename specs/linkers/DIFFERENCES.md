# Linker output differences — TLINK vs MS LINK (EXE-level toolchain fingerprints)

The decompiler-facing goal is to identify the compiler/toolchain of a **linked
executable**. Where an `.OBJ`'s vendor shows in COMENTs and OMF structure (see
[`../FINGERPRINTS.md`](../FINGERPRINTS.md), [`../MSC_FINGERPRINTS.md`](../MSC_FINGERPRINTS.md)),
a `.EXE`'s vendor shows in **how it was linked and what runtime got pulled in**.
This file catalogs the differences between Borland **TLINK 4.0** and Microsoft
**LINK** output, graded by confidence and — importantly — by *where the signal
comes from* (the linker itself vs the runtime library it linked in), because
those survive different kinds of tampering.

## The experiment

Same trivial source — `int main(void){return 0;}` — compiled+linked through both
real toolchains under the oracle (faketime-pinned, small model):

- `bcc -LC:\LIB HELLO.C`  → `HELLO.EXE` via TLINK 4.0
- `cl HELLO.C`            → `HELLO.EXE` via MS LINK (CL reads `LIB`; BCC needs `-L`)

(First linked-EXE capture; everything below is from this one fixture — grades are
provisional until more programs/models widen them.)

## Tier A — runtime-library signals (DEFINITIVE id, but stripped by repacking)

These come from the **startup module + C runtime** the linker pulled in, not the
linker itself. Strongest for identification, weakest against deliberate stripping.

- **Vendor copyright string, verbatim in the image** (DEFINITIVE):
  - Borland: `Borland C++ - Copyright 1991 Borland Intl.`
  - Microsoft: `MS Run-Time Library - Copyright (c) 1987, Microsoft Corp`
- **Runtime error-message table** (STRONG):
  - Borland: plain English — `Null pointer assignment`, `Divide error`,
    `Abnormal program termination`.
  - Microsoft: coded `R6xxx` messages — `R6000`/`- stack overflow`,
    `R6003`/`- integer divide by 0`, `R6009`, plus `;C_FILE_INFO`, `<<NMSG>>`.
- **Entry-point startup stub (machine code at CS:IP)** (STRONG):
  - Borland C0: `ba a6 00  2e 89 16 35 02  b4 30 cd 21 …`
    = `mov dx,0A6h / mov cs:[0235h],dx / mov ah,30h / int 21h` (stash + DOS getver).
  - Microsoft crt0: `b4 30 cd 21  3c 02  73 02  cd 20  bf 53 00 …`
    = `mov ah,30h / int 21h / cmp al,2 / jae +2 / int 20h` (DOS≥2 guard, then init).

## Tier B — linker-intrinsic signals (survive even when strings are stripped)

These are the linker's own structural conventions in the MZ header/layout — the
true *linker* fingerprint, robust to string-stripping.

| MZ header field | TLINK 4.0 | MS LINK | signal |
|---|---|---|---|
| checksum (`e_csum`) | `0x0000` (left zero) | `0x7d5f` (computed) | **STRONG**: TLINK never fills the MZ checksum; MS LINK does. |
| reloc-table offset (`e_lfarlc`) | `0x3e` | `0x1e` | **STRONG**: MS LINK packs the reloc table right after the 28-byte header; TLINK leaves a ~0x20-byte gap. |
| initial SP (`e_sp`) | `0x0080` | `0x0800` | STRONG: different default stack reservation. |
| min alloc (`e_minalloc`) | `0x000d` | `0x0081` | STRONG: different BSS/heap paragraph reservation. |
| reloc segments | all in seg `0x0000`: `(1,0)(222,0)(272,0)` | spread across segs `0`/`0x87`: `(10,87)(35,0)(174,0)(158,87)` | STRONG: MS LINK's image uses multiple far segments even for a trivial program; TLINK keeps one. |
| entry `e_ip` | `0x0000` | `0x0018` | STRONG: different startup-stub placement within `_TEXT`. |
| file size | `0xeea` (3818) | `0x907` (2311) | WEAK on its own: TLINK's default link is larger here. |

## Tier B — `.MAP` segment table & symbol policy (linker-intrinsic)

The linker map (`bcc -M` / `cl /Fm`) exposes the linker's **segment ordering and
class names** directly — a high-signal structural fingerprint independent of any
runtime strings. From the `linking/multi-module` fixtures (4254–4257):

- **Segment class vocabulary** (DEFINITIVE for the linker family — the names are
  emitted by the compiler's segment directives but the *ordered table* is the
  linker's):
  - TLINK: `_TEXT/CODE`, `_FARDATA/FAR_DATA`, `_FARBSS/FAR_BSS`,
    `_OVERLAY_/OVRINFO`, `_1STUB_/STUBSEG`, `_DATA/DATA`, `_CVTSEG`/`_SCNSEG`,
    `_CONST/CONST`, `_INIT_/INITDATA`, `_EXIT_/EXITDATA`, `_BSS/BSS`,
    `_STACK/STACK`. The `OVRINFO`/`STUBSEG`/`INITDATA`/`EXITDATA` classes are
    Borland-specific.
  - MS LINK: `_TEXT/CODE`, `C_ETEXT/ENDCODE`, `NULL/BEGDATA`, `_DATA/DATA`,
    `CDATA/DATA`, then the `XIFB…XIFE` / `XIB…XIE` / `XPB…XPE` / `XCB…XCE`
    initializer-table classes, `CONST`, `_BSS`, `STACK`. The `C_ETEXT`/`NULL`
    guard segments and the `Xxx` init/term tables are Microsoft-specific.
- **Publics table layout**: TLINK prints `Address  Publics by Name` then
  `… Publics by Value`, segment:offset as `00A7:0000`, with `Abs` for absolute
  symbols (`__AHINCR`, `__AHSHIFT`). MS LINK's columns and value formatting
  differ. (Detailed column diff: TODO as the sample grows.)

## Communal vs definite tentative definitions (compiler→linker behavior)

A file-scope `int g;` with no initializer is a *tentative definition*. The two
toolchains resolve a same-named tentative def in **two** TUs differently
(fixture 4257):

- **MSC/CL**: emits each as a **communal `COMDEF`**; MS LINK merges them into one
  BSS slot → links clean (exit 0).
- **BCC/TLINK**: emits each as a **definite public** in `_BSS`; TLINK sees a
  **duplicate symbol** and fails the link (non-zero exit). So the BCC sibling of
  4257 captures the *compile* stage only — the duplicate-symbol behavior is the
  fingerprint, not a clean EXE.

This is a usable discriminator: a multi-TU program with shared uninitialized
globals links under MS LINK but not under a naive BCC `bcc *.c` — Borland code
relies on exactly one definition (or `extern`) per global.

## Why the A/B split matters for decompilation

A compiler-aware loader should check **both** tiers and report confidence
accordingly: a stock EXE matches Tier A *and* B; an EXE that's been repacked or
had its `.data` strings scrubbed may only match Tier B (the linker's structural
habits), much like a TLIB-stripped `.OBJ` only matches the structural OBJ tier.
The MZ header conventions (checksum-left-zero, reloc-table packing) are the most
tamper-resistant single discriminators.

## Open / next

- Separate **linker-intrinsic** from **model/runtime** variance: re-run across
  memory models (S/C/M/L) and a few program shapes (calls printf, has globals,
  multiple TUs) to see which Tier-B fields are constant vs model-dependent.
- Confirm the startup stub is the model-specific `C0<model>.OBJ` / MS crt0 we
  already reproduce byte-exact via the provisioner — then entry-stub matching can
  reuse those known bytes directly.
- Drive the **linkers directly** (`Tool::Tlink` / `Tool::Link`) on hand-built
  OBJs to isolate pure linker behavior from compiler-driver defaults. The
  `linking/multi-module` fixtures already widen the Tier-B sample via the
  driver (multiple TUs, real `.MAP`s); standalone-linker fixtures are the next
  bucket (see `fixtures/c/linking/README.md`).
- Grade these toward DEFINITIVE/STRONG/WEAK as the sample set grows; cite the
  fixture that demonstrates each.
