# Microsoft C 5.0 compiler fingerprints

A catalog of patterns Microsoft C 5.0 (`CL.EXE`, the C2-side codegen)
leaves in its compiled output, organized for the **recognizer** view:
"if you see X in a binary, how strongly does that suggest MSC 5.0 was
the compiler?"

This is the inverse of `specs/msc/OMF_OUTPUT.md`, which is the
encoder view ("here's what MSC emits"). The two cover the same
patterns; this doc emphasizes:

- **Distinctiveness** — does any other era compiler emit the same pattern?
- **Stability** — does the pattern change across MSC versions / options?
- **Where to look** — what part of the binary to scan for it.
- **Strength** — how much evidence does this pattern provide on its own.

Patterns are organized roughly from highest-confidence (file-level
invariants) to lowest (per-construct peepholes). Anything below the
**weak signal** threshold needs to be observed in combination with
others.

This catalog is built from the Phase-1 corpus (fixtures 4075–4144).
Each entry cites the fixtures that demonstrate it. Patterns observed
across multiple unrelated source shapes are stronger than ones pinned
by a single fixture.

## How to use this catalog

Companion to `specs/FINGERPRINTS.md` (the BCC catalog). A combined
recognizer for an unknown OBJ would:

1. Apply both BCC and MSC catalogs.
2. The two compilers' patterns are almost everywhere distinguishable —
   BCC's `?debug` records, `@1@0` labels, and TASM macro preamble vs
   MSC's `MS C` COMENT, `__acrtused` extern, and pre-emitted FIXUPP
   THREAD record make even the file-level signatures disjoint.

The patterns marked **DEFINITIVE** are strong enough that even one
sighting is essentially conclusive (assuming the binary wasn't
hand-edited). **STRONG** patterns are highly indicative but might
recur in other Microsoft tools of the era (MASM-emitted OMF, other
MS C versions). **WEAK** patterns are typical of the era and only
useful in aggregate.

---

## File-level signatures

When inspecting MSC OBJ output (`cl /c`), these patterns are present
in essentially every compiled file. The `cl /Fa` form (assembly
output) lands in a separate dialect not covered by this Phase-1
catalog — the Phase-1 corpus exercises only `cl /c /AS`.

### `MS C` translator-id COMENT (DEFINITIVE)

Every MSC OBJ begins with a `THEADR` then a `COMENT` (record `0x88`)
of class `0x00` carrying the literal ASCII string `MS C` (4 bytes,
no NUL). The full record bytes are:

```
88 07 00 00 00 4D 53 20 43 <chksum>
```

(record type `0x88`, length 7, payload `00 00 MS C`, then checksum).
Distinctive: BCC writes a Turbo-style `0` class comment; LINK-merged
TASM output omits the translator string entirely. Fixtures: every
MSC fixture has this; canonical 4075.

### `SLIBCE` default-library COMENT (DEFINITIVE for /AS, STRONG generally)

A second `COMENT` of class `0x9F` (default-library directive) tells
LINK to pull in `SLIBCE` — the small-model C runtime. The actual
library name varies by memory model:

| Model | Library     |
|-------|-------------|
| `/AS` | `SLIBCE`    |
| `/AC` | `MLIBCE` *(presumed; not yet captured)* |
| `/AM` | `MLIBCE` *(presumed)*                   |
| `/AL` | `LLIBCE`    *(presumed)*                |

Distinctive: BCC names different runtime libraries (`COS.LIB`, etc.).
Fixture 4075 et al. show `SLIBCE`.

### Pre-emitted FIXUPP THREAD record (DEFINITIVE)

Before any FIXUP-bearing LEDATA, MSC emits a 13-byte FIXUPP record
that registers four target threads and two frame threads:

```
9c 0d 00  00 03  01 02  02 01  03 04  40 01  45 01  <chksum>
```

Decoded:
- T0 → SEGDEF 3 (CONST)
- T1 → SEGDEF 2 (_DATA)
- T2 → SEGDEF 1 (_TEXT)
- T3 → SEGDEF 4 (_BSS)
- F0 → SEGDEF 1 (_TEXT)
- F1 → GRPDEF 1 (DGROUP)

Subsequent FIXUPs reference these by single byte (`9c` / `9d`).
BCC does not pre-emit THREAD records — it emits explicit
frame+target datum pairs in every FIXUP, which is one of the
clearest divergences. Fixtures: every MSC OBJ.

### Pre-LEDATA `0xA2 01` start-of-data COMENT (STRONG)

A `COMENT` of class `0xA2` with single-byte payload `0x01`
sandwiches the link-pass marker between the EXTDEF/PUBDEF/COMDEF
setup and the LEDATA bytes:

```
88 04 00  00 A2 01  <chksum>
```

The matching `A2 00` (end-of-data) is absent from Phase-1 captures
(single-pass), but the opening half is constant. BCC has no analogue.

### LNAMES table with leading empty name (STRONG)

MSC's `LNAMES` record starts with a single-byte empty name (`00`)
before the real entries — used as a reserved "name index 1 = empty"
slot. The names are then, in this order:

```
1: ""           (empty placeholder)
2: DGROUP
3: _TEXT
4: CODE
5: _DATA
6: DATA
7: CONST
8: _BSS
9: BSS
```

The empty leading entry is distinctive — BCC's LNAMES starts directly
with the first segment name and uses different conventions for the
debug-segment names.

### Segment scaffold: `_TEXT, _DATA, CONST, _BSS` (STRONG)

Four SEGDEFs in that order, all with ACBP `0x48` (align WORD, comb
PUBLIC). Lengths reflect the actual content. The combination of
exactly four segments named `_TEXT/_DATA/CONST/_BSS` plus the
DGROUP membership `{CONST, _BSS, _DATA}` (in that order, with `CONST`
first) is MSC-specific — BCC orders DGROUP as `{_DATA, _BSS}` (no
CONST), and Borland never names a `CONST` segment in this codegen.

### GRPDEF: `DGROUP { CONST, _BSS, _DATA }` (STRONG)

The `GRPDEF` is `9A 08 00 02 FF 03 FF 04 FF 02` (chksum). Member
list: `_DATA-name-idx, _BSS-name-idx, CONST-name-idx`. The order
isn't source-derivable — MSC always puts CONST before _BSS before
_DATA in DGROUP.

### `__acrtused` startup symbol (DEFINITIVE)

Every MSC OBJ pulls in `__acrtused` as an EXTDEF entry, type-index
`0x01`. The double leading underscore plus the `crt used` etymology
identifies this as a MSC C runtime startup marker. No other compiler
of the era uses this exact name. Fixtures: every MSC OBJ.

### `__chkstk` stack-probe call (DEFINITIVE in combination with prologue)

Every function body opens with `xor ax, ax` (or `mov ax, frame_size`)
followed by `call __chkstk`. The call is always emitted, even for
zero-local frames — there is no threshold below which MSC elides the
probe. This contrasts with BCC's pattern of always emitting
`enter`/`leave` (on 286+ targets) or a bare prologue. Pairing the
`__acrtused` symbol with the `__chkstk` external call in the same
TU is essentially conclusive.

---

## Function-level structural signatures

### Prologue varies with frame contents (STRONG)

Three observed prologue shapes, distinguished by what AX carries
into `call __chkstk`:

| Frame              | Prologue bytes                              |
|--------------------|---------------------------------------------|
| No locals, no params | `33 c0 e8 .. ..`                          |
| Params only        | `55 8b ec 33 c0 e8 .. ..`                   |
| Locals (frame slide) | `55 8b ec b8 <size_lo> <size_hi> e8 .. ..` |

The `b8 <imm16> e8` mov-then-call sequence is the most distinctive —
it passes the local-frame byte count to `__chkstk` so the helper can
probe-down through the stack. Fixtures 4099 (no locals), 4080 (small
frame), and longer local lists confirm.

### `xor ax, ax` strictly for chkstk arg (STRONG)

When MSC needs AX=0 it picks **`33 c0` (xor) only when feeding
`__chkstk`** — i.e., as the prologue's immediate predecessor of `e8`.
For zeroing AX as a return value, MSC uses `2b c0` (sub ax, ax). Both
are 2 bytes but distinguishable. Seeing `33 c0` everywhere else
(e.g., as a return-value emit, without a following call) suggests a
different compiler.

### `2b c0` for `return 0;` (DEFINITIVE)

MSC's return-zero is `2b c0 c3` (sub ax,ax; ret). No other era
compiler we've catalogued picks SUB over XOR for this idiom. BCC
uses `xor ax, ax`. This single 2-byte sequence trailing a `c3`
strongly suggests MSC.

### Epilogue (STRONG)

| Frame              | Epilogue                            |
|--------------------|-------------------------------------|
| No locals/params   | `c3`                                |
| Params only        | `5d c3`                             |
| Locals (frame slide) | `8b e5 5d c3` (mov sp,bp; pop bp; ret) |

`8b e5 5d c3` is the MSC tell — BCC's frame-slide epilogue uses
`leave` (a single-byte instruction) on 286+ targets.

### NOP padding to even byte count (STRONG)

When the natural function body ends at an odd byte boundary, MSC
appends a single `0x90` (NOP) so the next function or data starts
even. BCC has the same even-padding habit but uses different markers
for chained functions.

---

## OMF record ordering signatures

### PUBDEF source-order with bucket splits (STRONG)

MSC walks declarations in source order and starts a new PUBDEF
record on each `(group, segment)` transition. So `int g; void f();
int main() {...}` yields **three** PUBDEFs: `_g` in DGROUP:_DATA,
then `_f` in 0:_TEXT, then `_main` in 0:_TEXT (the last two combine
into one record only when they're adjacent in source). Fixture 4125
pins this.

BCC pre-sorts PUBDEFs (length-asc then reverse-alpha) — they're
never split this way.

### COMDEF sandwich (STRONG)

When tentative globals (`int g;`, `char s[N];`) exist alongside
externs, MSC emits:

1. EXTDEF #1: `__acrtused`, `__chkstk`
2. COMDEF: tentative globals (type `0x62` = NEAR data)
3. EXTDEF #2: function names

The COMDEF length is encoded **single byte for ≤0x80**, otherwise
`0x81` then little-endian u16. BCC uses BSS pubdefs for the same
construct — no COMDEFs at all. Fixtures 4105, 4107, 4114.

### CONST string-pool LEDATA layout (STRONG)

Each string literal lands in `CONST`, packed into LEDATAs at 2-byte
aligned offsets. Odd-length strings leave a 1-byte zero hole and
force the next LEDATA to open at a new offset; even-length strings
allow the next string to concatenate into the current LEDATA. The
length field in the LEDATA never crosses a string boundary that
required padding. Fixtures 4113 (two odd → 2 LEDATAs), 4128 (two
odd different content), 4132 (even+odd → 1 LEDATA).

BCC interns strings in `_DATA` under `s@`-prefixed labels with no
alignment padding.

### LEDATA + FIXUPP layering (STRONG)

MSC's LEDATA emission order:

1. **CONST** LEDATAs (one or more, depending on padding).
2. **_DATA** LEDATA → immediately followed by a FIXUPP record if any
   slot is a `StrAddr` / `GlobalAddr` pointer-to-something.
3. **_TEXT** LEDATA → immediately followed by a FIXUPP record for
   the instruction-level fixups (`ExtCall`, `StrLoad`, `GlobalAddr`).

The interleaved-FIXUPP-after-each-LEDATA pattern is distinctive —
BCC batches all FIXUPs into one record at the end. Fixture 4110 is
the canonical multi-LEDATA case.

### FIXUP subrecord menu (STRONG)

| Bytes              | Means                                  |
|--------------------|----------------------------------------|
| `c4 off 9c`        | DGROUP frame + CONST target, no disp   |
| `c4 off 9d`        | DGROUP frame + _DATA target, no disp   |
| `c4 off 56 idx`    | target's frame + explicit EXTDEF idx   |
| `84 off 56 idx`    | self-rel + explicit EXTDEF idx         |

These four byte sequences are MSC-specific because of the pre-emitted
THREAD record they implicitly reference. BCC's FIXUPs are always
explicit-frame, explicit-target (longer subrecords).

### FIXUPP descending-offset sort (STRONG)

Within each FIXUPP record, subrecords are sorted by **descending**
LEDATA offset. Fixture 4103 (3 fixups in one _TEXT FIXUPP),
fixture 4113 (2 fixups in _DATA FIXUPP). BCC's batched FIXUPP uses
emission-order, not sorted.

---

## Codegen peephole signatures

### `inc word ptr [g]` for `g = g + 1;` on a global (STRONG)

MSC peepholes `g = g + 1;` and `g = g - 1;` into in-place memory
ops: `ff 06 <addr>` (inc) / `ff 0e <addr>` (dec). 4 bytes total.
Fixture 4141 (decrement in a while body), 4142 (increment then
read). The contrasting "read-modify-write" form (`a1 <addr>; 40;
a3 <addr>`) is 7 bytes — MSC always picks the 4-byte form when
the source matches.

### `cmp word ptr [g], K` with `83 3e ...` shape (STRONG)

`if (g == K)` and `if (g)` use the `83 3e <addr> imm8` cmp-imm
shape. The displacement is a 16-bit address (mod=00, r/m=110);
the immediate is sign-extended byte. For K not fitting in an i8,
MSC would switch to `81 3e <addr> imm16` (not yet exercised by
the Phase 1 corpus). Fixtures 4129 (g vs 0), 4133 (g vs 5).

### Global-arithmetic uses memory operand (STRONG)

`a + b` where both are globals emits `a1 <addr_a>` then `03 06
<addr_b>` (4-byte `add ax, word ptr [imm16]`). MSC never spills
to a temporary — it uses the memory operand directly. Fixture
4138. Same idiom for sub (`2b 06 <addr>`).

### `+ 1` strength-reduces to `inc ax` (STRONG)

`g + 1`, `x + 1`, `K + 1` all peephole to `40` (inc ax) when the
running value is already in AX. Same for `- 1` → `48` (dec ax).
Fixtures 4135 (global + 1 in a return), 4082 (local + 1).

### `* 2` → `shl ax, 1` (STRONG)

Single-bit shift via `d1 e0`. `* 3` → 6-byte `mov cx, ax; shl ax, 1;
add ax, cx` shift-and-add (fixture 4088). BCC also strength-reduces
`*2` but spreads the patterns across more shift widths.

---

## Pointer / array signatures

### Constant-index array read: direct addressing (STRONG)

`return a[K];` where a is a global int array lowers to `a1
<byte_off>` with a FIXUP — the index is folded into the placeholder
bytes (`byte_off = K * elem_size`). Fixture 4109 (int array),
4121 (char array using `a0 <off> 98` form for the byte load + cbw).

### Variable-index array read: BX-scaled (STRONG)

`return a[i];` (i a param) lowers to `8b 5e <disp>; d1 e3; 8b 87
<addr_lo> <addr_hi>` — load i, shl-by-1, then `mov ax, [bx +
addr16]`. The `87` ModR/M (mod=10, reg=000 (ax), r/m=111 (bx +
disp16)) is the signature byte. Fixture 4112.

### Char pointer deref: `mov bx, [p]; mov al, [bx]; cbw` (STRONG)

`return *p;` where p is `char *` always uses this exact sequence
(`8b 1e <addr>; 8a 07; 98`). The `cbw` (`98`) widens AL → AX for
the int return. Fixture 4111, 4123 (with disp8: `8a 47 <K>`).

### Int pointer deref via param: `mov bx, [bp+4]; mov ax, [bx]` (STRONG)

`*p` where p is an `int *` parameter lowers to `8b 5e 04; 8b 07`.
No `cbw` — the read is already word-wide. Fixture 4125.

### Pointer-init shape (STRONG)

`char *p = "hi";` lands the pointer slot in `_DATA` with a
FIXUP-encoded CONST offset placeholder. The string lives in
CONST. `int *q = &g;` writes g's `_DATA` offset into q's slot, with
a `c4 off 9d` FIXUP. Fixtures 4110, 4115.

### `b8 <addr> 50` for `&g` as a call arg (STRONG)

When `&g` appears in an argument position, MSC emits `b8 00 00; 50`
(`mov ax, addr; push ax`) plus a `GlobalAddr` FIXUP on the imm16.
4 bytes. Fixture 4125.

### `ff 36 <addr>` for global as a call arg (STRONG)

`f(g)` pushes the global directly with `ff 36 <addr>` (push word
ptr [imm16]) — no register temporary. 4 bytes. Fixture 4131.

---

## Calling-convention signatures

### Standard cdecl push-RTL + `add sp, N` cleanup (WEAK alone, STRONG combined)

Args pushed right-to-left; caller cleans with `83 c4 imm8` for
total byte counts ≤127. Zero-arg calls skip cleanup entirely (no
`83 c4 00`). The byte count is always `2 * argc` because every Phase
1 fixture pushes 2-byte values. Combined with the other MSC
signatures, this confirms small-model cdecl.

### Cleanup uses `83 c4 imm8sx`, not `pop` instructions (STRONG)

BCC of similar vintage uses `pop cx` for single-arg cleanups; MSC
always uses `add sp, N` even for `N=2`. Fixtures 4099 (zero args,
no cleanup), 4100 (one arg, `83 c4 02`).

### Constant int arg: `b8 K K 50` (STRONG)

`mov ax, imm16; push ax`. 4 bytes. Distinctive in combination with
the LE byte order and lack of any optimization to `push imm8sx`
(MSC 5.0 doesn't have `push imm8` on the 8088 target). Fixture 4100.

### Function-local calls: in-band self-rel patched (DEFINITIVE)

When `f` calls `g` and both live in the same TU, MSC patches the
`e8 disp disp` displacement in the OBJ itself — no FIXUPP record
is emitted for the intra-TU call. Externs (`printf`, etc.) and
runtime helpers (`__chkstk`) route through the EXTDEF + FIXUPP
machinery. Fixture 4101 (intra-TU call), 4103 (extern call).

---

## Control-flow signatures

### `while`/`for` trampoline jumps to the cond, not the body (STRONG)

MSC's loop layout: `eb <disp>` jumps forward to the cmp; body
inline; cmp inline; `jcc <-disp>` jumps backward into the body.
Optional `90` NOP pad after the initial `jmp` keeps the body
even-aligned. Fixtures 4096 (while), 4097 (for), 4141 (while-cond
on global).

### NOP pad after forward jmp when offset would be odd (STRONG)

If the byte right after the 2-byte `eb XX` would land at an odd
address, MSC inserts a single `0x90`. The body's branch
displacements account for the pad. BCC also pads but uses a different
trigger condition.

### `do-while` flag-elision (DEFINITIVE)

When the loop body's last instruction already sets ZF for the cond
(`x = x - 1;` paired with `while (x);`), MSC elides the explicit
`cmp` and chains the backward `jcc` directly off the body's flags.
Fixture 4098. BCC keeps the cmp.

---

## Optimizer / const-prop signatures

### Straight-line const-prop across globals and locals (STRONG)

`g = 5; return g;` lowers to `c7 06 <addr> 05 00; b8 05 00; c3` —
the second-use of `g` is replaced with the just-stored constant,
not a memory reload. Same for locals (`i = 2; return a[i];` becomes
`a[2]` direct). Fixtures 4106 (global), 4112 (local).

### Const-prop clears on control flow (STRONG)

Across `if`, `while`, `for`, `do-while` the known-value table
resets. Inside straight-line statement sequences within one basic
block, every literal-RHS assignment is propagated forward. This is
why fixtures need parameter-indexed array access to defeat the
optimizer — locals get folded.

---

## Default catalog versions / library text

Strings worth scanning a binary for:

- `MS C` (translator-id COMENT)
- `SLIBCE` / `MLIBCE` / `LLIBCE` (default-lib COMENT, by model)
- `__acrtused` (EXTDEF — always)
- `__chkstk` (EXTDEF — always)

Seeing two or more of these together is essentially conclusive
evidence of an MSC-compiled OBJ.

---

## Open questions for further fingerprinting

These would each warrant a dedicated probe:

- **`switch` strategy** — Phase 1 didn't exercise switch. MSC likely
  emits a jump table for dense cases and a sequential `cmp/je` chain
  for sparse — but the threshold and the indirect-jump shape need
  fixtures.
- **`long` (32-bit) arithmetic** — fxn/library ABI for `__udiv4`,
  `__umul4`, etc. BCC's analogous F77 ABI is heavily catalogued
  (see `specs/FINGERPRINTS.md` long-arithmetic section); MSC's
  needs the same treatment.
- **Float / double codegen** — separate emulator library shape,
  inline 8087 alternate. MSC 5.0 shipped with both math libraries.
- **`/Ox` peephole differences** — what specifically does `/O2`
  change vs the unoptimized output Phase 1 used.
- **`#pragma` recognition** — segment / alloc_text / code_seg.
- **`register` keyword** — does MSC pick the same locals BCC
  promotes? The threshold and register pool order are open.
- **Other memory models** — `/AC`, `/AM`, `/AL` segment naming and
  pointer sizes. Phase 1 only covered `/AS`.
