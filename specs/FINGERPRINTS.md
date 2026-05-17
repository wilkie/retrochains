# BCC 2.0 compiler fingerprints

A catalog of patterns BCC 2.0 leaves in its compiled output, organized
for the **recognizer** view: "if you see X in a binary, how strongly
does that suggest BCC 2.0 was the compiler?"

This is the inverse of `specs/bcc/ASM_OUTPUT.md`, which is the
encoder view ("here's what BCC emits"). The two cover the same
patterns; this doc emphasizes:

- **Distinctiveness** — does any other era compiler emit the same pattern?
- **Stability** — does the pattern change across BCC versions / options?
- **Where to look** — what part of the binary to scan for it
- **Strength** — how much evidence does this pattern provide on its own

Patterns are organized roughly from highest-confidence (file-level
invariants) to lowest (per-construct peepholes, where single
instances can be coincidental). Anything below the **"weak signal"**
threshold needs to be observed in combination with others.

## How to use this catalog

The `crates/fingerprint/` tool is the start of this recognizer. A fuller
fingerprinter would:

1. Disassemble a target binary (`.exe`, `.obj`, `.asm`).
2. Search for each pattern below, accumulating evidence weights.
3. Combine weights into a posterior over candidate compilers.

The patterns marked **DEFINITIVE** are strong enough that even one
sighting is essentially conclusive (assuming the binary wasn't
hand-edited). **STRONG** patterns are highly indicative but might be
shared with closely related toolchains (other Borland products of the
era). **WEAK** patterns are typical of the era and only useful in
aggregate.

The catalog is a living document: entries grow as fixtures accumulate.
Each entry cites the fixture(s) that demonstrate it.

---

## File-level signatures

When inspecting BCC's `.asm` output (the `-S` form) or the closely
matching `.obj` output, these patterns are present in essentially
every compiled file.

### Macro preamble (DEFINITIVE)

The exact 14-line preamble:

```
	ifndef	??version
?debug	macro
	endm
publicdll macro	name
	public	name
	endm
$comm	macro	name,dist,size,count
	comm	dist name:BYTE:count*size
	endm
	else
$comm	macro	name,dist,size,count
	comm	dist name[size]:BYTE:count
	endm
	endif
```

The `??version` symbol probe is specific to recent Borland TASM. The
exact wording, indentation (tab, not spaces), and the `publicdll`
trampoline name are distinctive. _Stability_: invariant across all
fixtures we've captured. _Where_: top of any `BCC -S` output.

### `?debug` records (DEFINITIVE for .asm; signature in .obj too)

```
	?debug	S "hello.c"
	?debug	C E9006097160768656C6C6F2E63
```

- The `?debug S` record carries the source filename, **lowercased**
  even when the user passed `HELLO.C`.
- The `?debug C` comment record begins with byte `0xE9` (record
  subtype), followed by a 4-byte little-endian DOS-packed mtime, a
  1-byte filename length, then the filename ASCII.

The combination of `?debug S` + `?debug C E9...` is a Borland-specific
debug record format. Other DOS-era compilers use COFF/CodeView or no
debug records at all. _Stability_: invariant. _Where_: just after the
macro preamble.

### Segment scaffold (STRONG)

```
_TEXT	segment byte public 'CODE'
_TEXT	ends
DGROUP	group	_DATA,_BSS
	assume	cs:_TEXT,ds:DGROUP
_DATA	segment word public 'DATA'
d@	label	byte
d@w	label	word
_DATA	ends
_BSS	segment word public 'BSS'
b@	label	byte
b@w	label	word
_BSS	ends
```

The `d@` / `d@w` / `b@` / `b@w` section-base labels are
BCC-specific. Other compilers use different conventions (`_data_start`,
`__bss`, etc.). The `byte`/`word` alignment specifiers and the
`DGROUP` grouping are also typical of Borland's segmented model.

### `_TEXT` opens once per translation unit (STRONG)

Multi-function source files have a single `_TEXT segment byte public
'CODE'` near the top of the function region, all functions inside,
then a single `?debug C E9` followed by `_TEXT ends`. Compilers that
emit one segment per function (e.g., to make linking-out unused
functions easier) would be distinguishable by this. _Fixture_: 009.

### `public` symbol ordering (STRONG, unresolved general rule)

```
	public	_main
	public	_f
```

The `public` list at end-of-file is **not** source order. Early
fixtures (009, 010, 087) could be explained as reverse declaration
order, but fixture 095 (`sum` defined before `main`, emitted as
`sum, main`) proves source order is insufficient.

For the committed fixture corpus, our emitter currently matches BCC with
a length-bucket plus reverse-alphabetical approximation. Targeted probes
outside that corpus show the real rule is more likely hash-bucket based;
see `specs/OPEN_QUESTIONS.md`. Treat the non-source-order public list as
a strong fingerprint, but do not treat plain reverse-alpha as a closed
fact.

### Function-symbol prefix (WEAK)

C function `foo` becomes `_foo` in asm. This is the standard cdecl
naming convention shared with many DOS / 16-bit Windows compilers
(MSC, Watcom, etc.), so on its own it's not distinctive.

### Output filename and source casing (STRONG, for .asm)

Output `<input-basename>.ASM` (uppercase), with the `?debug S`
filename **lowercased**. This casing split is a fingerprint of BCC's
file handling. _Fixture_: 001.

### `0x1A` (DOS EOF) terminator (WEAK)

The `.asm` file ends with the DOS Ctrl-Z byte. Common in DOS-era
toolchains; not distinctive on its own.

### CRLF line endings, TAB indentation, decimal immediates (WEAK)

All era-typical. Useful as a sanity check but not as a discriminator.

---

## Function-level structural signatures

### Prologue (STRONG)

```
	push	bp
	mov	bp,sp
	[dec sp / sub sp,N — only if locals]
	[push si — only if SI used]
	[push di — only if DI used]
	[mov <reg>, <ptr> [bp+N] — once per register-promoted param]
```

The strict order — stack allocation **before** the callee-saved pushes,
and DI **always** pushed after SI — is BCC's specific shape. Many other
era compilers push registers before allocating locals, or save the
register set in a different order. _Fixtures_: 035, 048.

### `dec sp` vs `sub sp` threshold at 2/3 bytes (STRONG)

A 2-byte frame uses `dec sp / dec sp` (2 single-byte instructions); a
4-byte frame uses `sub sp, 4` (3-byte instruction). The crossover is
based on encoded byte count: BCC always picks the shorter encoding.
This *exact* crossover point is a signature — some compilers always
use `sub sp, N`, others always use `add sp, -N`. _Fixtures_: 004, 006.

### Frame padded to even byte count (STRONG)

A single-byte `char` frame is allocated as 2 bytes (`dec sp / dec sp`,
not just one `dec sp`). _Fixture_: 055.

### Single exit label per function via `jmp short` (DEFINITIVE)

Every `return` becomes `jmp short @<n>@<slot-label>` to a single exit
label, where the epilogue lives. In straight-line functions that label
is `@<n>@50`; in functions that reserve earlier label slots it is a
later `50 + 24*slot` value. Even an unconditional `return 0;` in a
straight-line function still goes through this `jmp`. Most other
compilers inline the `ret` at each return site.

### Epilogue (STRONG)

```
@<n>@50:
	[pop di / pop si — reverse of prologue]
	[mov sp,bp — only if there were stack-resident locals]
	pop	bp
	ret
```

The conditional `mov sp,bp` (present iff stack-resident locals are
present) is a BCC quirk — many compilers always emit it. _Fixture_: 030.

### `ret` with trailing TAB+CRLF (STRONG, .asm-specific)

All operand-less mnemonics in BCC's `.asm` output are emitted as
`<mnemonic>\t\r\n` — a trailing tab where an operand would go. This is
visible in `.asm` output (`cbw`, `cwd`, `ret`). Other assemblers'
emitters often skip the trailing whitespace.

---

## Label numbering signature

### `@<func-idx>@<50 + 24*slot>` (DEFINITIVE)

Every local label follows this pattern. The base of 50 and step of 24
are unique to BCC. No fixed-base/step scheme of other era compilers
matches.

Concrete numbers:
- First slot: `@1@50`
- Second: `@1@74`
- Third: `@1@98`
- Fourth: `@1@122`
- Fifth: `@1@146`

A binary with these specific label numbers is essentially conclusive
proof of BCC 2.0. _Fixtures_: all `-S` fixtures.

### Function index increments source-order (STRONG)

The first function defined gets `@1@…`, the second `@2@…`, etc. This
combines with the non-source-order public-symbol list as a cross-check.

---

## Register-allocation signatures

These show up in the *flow* of variables through registers, even
without source-level visibility. A disassembler doesn't need to
recover the C source to see them — the asm itself reveals them.

### 5-int register pool: `{SI, DI, DX, BX, CX}` (STRONG)

BCC enregisters up to 5 `int` locals/params at once into these
registers. _Fixtures_: 048 (six eligibles, one spills).

### SI for the most-used int (STRONG)

The eligible int with the highest textual use count gets SI. Ties
broken by source order. _Fixtures_: 031, 035, 046, 048, 066.

### Remaining ints fill `[DI, DX, BX, CX]` in source order (STRONG)

After SI is assigned, the rest take from this fixed ordered list.

### Char register pool: `{DL, BL, CL}` (STRONG)

Chars use a separate 8-bit-register pool. Note: AL is not used (it's
the working byte); AH/BH/CH/DH are not used at all. _Fixture_: 050.

### Chars don't enregister when the function makes a call (STRONG)

DL/BL/CL alias with the caller-clobbered halves of DX/BX/CX. BCC keeps
chars on the stack in any function that contains a `Call` expression.
_Fixture_: 055.

### `≥ 3 textual uses` threshold (STRONG)

A local needs at least 3 textual occurrences (including its
initializer, but **not** an uninitialized declaration) to qualify for
register allocation. Below 3, it goes to the stack. _Fixture_: 030
(limit with 2 uses on stack), 032 (i with 3 uses → SI).

### Callee-saved: SI and DI only (STRONG)

Functions that use SI or DI emit `push si` / `push di` in the prologue
and the corresponding pops in the epilogue. Functions that only use
DX, BX, or CX for variables emit *no* saves — those registers are
treated as caller-clobbered. _Fixture_: 046.

---

## Peephole signatures

Each of these is a small but real BCC-specific idiom. Individually
weak signals; collectively very strong, especially because BCC applies
them consistently.

### `xor ax, ax` for zero into AX (WEAK)

Universal era convention. Almost every compiler does this. Not on
its own a discriminator.

### `xor <reg>, <reg>` for zero into a non-AX 16-bit reg (STRONG)

`int x = 0;` where x is in SI emits `xor si, si`, not `mov si, 0`.
But notably **NOT applied to 8-bit registers** — `char c = 0;` is
`mov dl, 0`, never `xor dl, dl`. This asymmetry is distinctive.
_Fixtures_: 027, 047, 050.

### `inc ax` / `dec ax` for `±1`, doubled for `±2` (STRONG)

`<reg> = <reg> ± K` (full assignment) goes through AX. For K = 1
the rhs collapses to `inc ax` / `dec ax`; for K = 2 BCC emits the
operation *twice* (`inc ax / inc ax`) — 2 bytes vs. 3 for `add ax, 2`.
At K ≥ 3 the cost tied and BCC switches to `add ax, K` / `sub ax, K`
(3 bytes). _Fixtures_: 027–031 (K=1), 076 case 1 (K=2), 076 case 2
(K=3 with `add`).

### `inc <reg>` / `dec <reg>` directly for `++x` / `--x` / `x += 1` (DEFINITIVE)

When the target sits in a register, BCC emits a *single* `inc <reg>`
instruction for `++x;`, `x++;`, `x += 1;`. This bypasses the AX
round-trip that a plain `x = x + 1;` would use. _Fixtures_: 040, 067.

This is one of the strongest fingerprints: very few era compilers
chose to specialize ++/+=1 separately from generic `x = x + 1`.

### `add <reg>, K` direct for compound assignment (DEFINITIVE)

`x += K;` with x in a register emits `add <reg>, K` directly, vs.
`x = x + K;` which emits `mov ax, <reg> / add ax, K / mov <reg>, ax`.
The two source forms are semantically identical but produce
distinguishable asm. _Fixtures_: 068, 070.

This is the same kind of signal as `inc <reg>` above but covers all
compound operators (`-=`, `&=`, `|=`, `^=`).

### `or <reg>, <reg>` for `cmp <reg>, 0` (STRONG)

When BCC compares a register-resident value against zero, it uses
`or <reg>, <reg>` instead of `cmp <reg>, 0`. The flag effects are
identical for the signed jumps that follow. Two bytes vs three.
_Fixture_: 035.

### `neg ax / sbb ax, ax / inc ax` for `!x` (DEFINITIVE)

The 4-instruction zero-test idiom for logical NOT is distinctive —
many compilers use `cmp ax, 0 / sete <reg>` (80386+) or a
comparison-as-value pattern. BCC's classic 8086-only sequence is rare
outside Borland tools. _Fixture_: 038.

### Single-operand `imul <src>` (STRONG)

BCC stays on the 8086-compatible single-operand `imul src` (`DX:AX
:= AX * src`). Compilers targeting 80186+ often use two-operand
`imul reg, src, imm`. _Fixture_: 008.

### `cwd / idiv <src>` for signed division (STRONG)

The `cwd` (convert word to doubleword) before every `idiv` is a
Borland habit — some compilers use `cwd` only when the dividend could
be negative, BCC always emits it. _Fixture_: 012.

### Shifts always via CL (STRONG)

`x << K` with constant K still loads CL: `mov cl, byte ptr [bp-N] /
shl ax, cl`. The 80186+ `shl reg, imm` form is avoided. Even when the
shift count is a compile-time constant, BCC reads it as a byte from a
local. _Fixture_: 017.

### `mov al, dl / cbw` for char-to-int widening (STRONG)

Reading a char into AX is `mov al, <byte-source> / cbw`. The `cbw`
sign-extends AL into AX. _Fixture_: 053.

### Constants emit as unsigned-wrapped decimal (STRONG)

`-5` emits as `mov ax, 65531`, not `mov ax, -5`. The asm dialect
forces unsigned representation, and BCC narrows to 16 bits before
formatting. _Fixture_: 036.

---

## Long-arithmetic signatures

These patterns only appear when the source uses `long` / `unsigned
long`. A binary that emits none of them was either compiled from
int-only source or pre-folded all 32-bit work into `int` pairs.

### Long memory storage: low word at `+0`, high word at `+2` (STRONG)

A 32-bit `long` global occupies 4 bytes laid out little-endian by
word: `_g+0`/`_g+1` is the low half, `_g+2`/`_g+3` is the high half.
Stack-resident longs follow the same convention — for a `long`
parameter the low half is at `[bp+N]` and the high half is at
`[bp+N+2]`. This means a long is read with two separate `word ptr`
references whose offsets differ by 2. _Fixtures_: 204, 205, 217.

### Two register conventions, context-dependent (STRONG)

BCC uses *different* register-pair assignments for the high/low halves
of a long depending on where the value lives:

| Context              | High in | Low in | Reason                          |
|----------------------|---------|--------|---------------------------------|
| Global arithmetic    | **AX**  | **DX** | BCC's working-register pattern  |
| Stack/param arithmetic | **DX** | **AX** | Matches the return ABI         |
| Function return / helper call | **DX** | **AX** | Standard 16-bit cdecl long ABI |

A function returning `long 5` emits `xor dx,dx / mov ax,5` (fixture
212). A `g = g + 10` where g is global emits `mov ax,_g+2 / mov dx,_g
/ add dx,10 / adc ax,0` — AX is high (fixture 207). A `return a + b`
where both are long params emits `mov dx,[bp+6] / mov ax,[bp+4] / add
ax,[bp+8] / adc dx,[bp+10]` — DX is high (fixture 285). The context
swap is not seen in compilers that use one convention throughout.

### Long load order: high-half first (STRONG)

In both register conventions BCC loads the high half *before* the low
half. For globals: `mov ax,_g+2` then `mov dx,_g`. For stack params:
`mov dx,[bp+6]` then `mov ax,[bp+4]`. The arithmetic op then runs on
the low half first (`add dx, …` or `add ax, …`) and propagates carry
into the high half via `adc`. _Fixtures_: 207, 219, 285.

### Compound assigns: `+=`/`-=` use imm8sx, `&=`/`|=`/`^=` use imm16 (STRONG)

For 32-bit `long`, BCC emits a memory-direct read-modify-write pair
for compound assigns, **but the immediate encoding width is
op-family dependent**:

- `+=` / `-=` use Grp1 `r/m16, imm8sx` (opcode `83`) — `83 06 …` /
  `83 2E …` for globals, `83 46 dd` / `83 6E dd` for stack locals.
  The high partner is always `adc 0` / `sbb 0`, same width.
- `&=` / `|=` / `^=` use Grp1 `r/m16, imm16` (opcode `81`) — `81 26 …`
  / `81 0E …` / `81 36 …` for globals, `81 66 dd …` / `81 4E dd …`
  / `81 76 dd …` for stack locals. The high partner uses the same
  mnemonic against the high half of K (no carry/borrow involvement).
  **Even when the immediate trivially fits in an i8sx**, BCC stays on
  the wider form.

Concrete: `long g; g &= 15;` emits
```
and word ptr DGROUP:_g, 15        ; 81 26 lo hi 0F 00 (6 bytes)
and word ptr DGROUP:_g+2, 0       ; 81 26 lo hi 00 00 (6 bytes)
```
The 4-byte saving from picking `83 26 lo hi 0F` (5 bytes) is left on
the table. This isn't a TASM default — TASM's sign-extension heuristic
would pick the shorter form for `15` if BCC asked for it. BCC's
emission must specifically choose `81` for the bitwise compound
shapes, while picking `83` for the arithmetic siblings.

The same op-family selection holds for **long stack locals** — only
the ModR/M byte changes (mem8-form `46`/`56`/`5E`/`66`/`6E`/`76`
instead of mem16-form `06`/`16`/`1E`/`26`/`2E`/`36`), the
imm8sx-vs-imm16 width choice is preserved. So a compound assign
fingerprint applies whether the target lives in `DGROUP` or on the
stack — useful when disassembling a function with no symbol info.

Distinct from slice-207-style `g = g <op> K` which routes through
registers entirely. _Fixtures_: 251 (global `+=` → 83), 253 (global
`&=` → 81), 288 (stack `+=` → 83), 289 (stack `&=` → 81).

### Long zero-compare via OR-then-test (STRONG)

`if (g == 0)` for a long `g` lowers to `mov ax,_g / or ax,_g+2 / jne
…`. The OR collapses the high and low halves into AX; if either was
nonzero, ZF=0. Two memory loads and one OR instead of two separate
compares against zero. Bare-long truth tests (`if (g)`) use the same
shape. _Fixture_: 215.

### 3-jump signed-long compare (STRONG)

`if (a < b)` for two longs lowers to a high-half compare followed by
a low-half compare, with **mixed signed/unsigned mnemonics**:

```
mov ax,_a+2
mov dx,_a
cmp ax,_b+2          ; high halves, signed
jg  short @false     ; a.high > b.high → not less
jl  short @true      ; a.high < b.high → less
cmp dx,_b            ; equal high halves → low halves, UNSIGNED
jae short @false     ; a.low >= b.low → not less
@true:
```

The high cmp is signed (`jl`/`jg`), the low cmp is unsigned (`jb`/
`ja`/`jae`/`jbe`) — necessary because the low word is conceptually a
magnitude, not a signed value. For `unsigned long`, both halves use
unsigned mnemonics. The "three jumps for one compare" shape is a
strong tell — most compilers would helper-call out to a `long_cmp`
runtime. _Fixtures_: 234–241.

### `long g = g * 2` peepholed to `shl/rcl` (STRONG)

`g = g * 2` (or any power-of-two-by-2 doubling) on a long global
emits `mov ax,_g+2 / mov dx,_g / shl dx,1 / rcl ax,1 / mov _g+2,ax /
mov _g,dx` — no helper call. The `shl/rcl` pair propagates the carry
bit from the low half into the high half. Distinguishable from the
general multiply path, which goes through `N_LXMUL@`. _Fixture_: 283.

### Compound shift K>1 reorders `mov cl, K` to come first (STRONG)

For `long a <<= K` with K>1, BCC emits `mov cl, K` **before** loading
the long into DX:AX, not after as you might expect. The full shape:
```
mov cl, 2
mov dx, word ptr DGROUP:_a+2
mov ax, word ptr DGROUP:_a
call near ptr N_LXLSH@
mov word ptr DGROUP:_a+2, dx
mov word ptr DGROUP:_a, ax
```
Compare with the non-compound form `a = a << K` (slice equivalent),
which loads DX:AX *before* `mov cl`. The shift count establishes CL
before the helper-input registers are touched, perhaps to leave DX:AX
"hot" up to the call. _Fixtures_: 263 (`<<=`), 264 (`>>=`).

### Runtime helpers declared `:far`, merged into publics sort (STRONG)

Long multiply/divide/mod/shift route through pre-built runtime
helpers in the Borland CRT: `N_LXLSH@`, `N_LXRSH@`, `N_LXURSH@`,
`N_LXMUL@`, `N_LDIV@`, `N_LMOD@`, `N_LUDIV@`, `N_LUMOD@`. The helper
name is suffixed with `@`, and the extern declaration uses **`:far`**
(BCC's user-function externs use `:near`):
```
extrn N_LXLSH@:far
```
Helpers take their args as a `DX:AX` long (or two `DX:AX` longs
pushed on the stack), and return their result in `DX:AX`. No caller
stack cleanup is emitted — the helper pops its own arguments (likely
via `ret 8`).

The helper extern is **not** segregated into its own block — it
joins the publics list at end-of-file, participating in the same
length-bucket + reverse-alphabetical sort used for ordinary publics.
A typical tail (`a <<= 2`):
```
public _main
extrn N_LXLSH@:far
public _a
```
Both the `:far` suffix and the merge-into-publics ordering are
distinctive. _Fixtures_: 232 (`_LDIV@`), 263 (`N_LXLSH@`).

### Long params occupy 4 stack bytes, push high-first (STRONG)

A `long` argument is pushed as two `push <reg>` instructions, **high
half first**, so the low half ends up at the lower bp-offset in the
callee. A constant `long 5` is materialized as `xor ax,ax / mov dx,5
/ push ax / push dx` (fixture 216). On the callee side, the first
long parameter is read as `mov ax,[bp+4] / mov dx,[bp+6]` — `[bp+4]`
low, `[bp+6]` high.

Combined with the per-arg cleanup rule, a single-long call cleans up
with two `pop cx` (2 args' worth of words), not one. _Fixtures_: 216,
217, 285.

### Non-constant long args push memory-direct (STRONG)

When the long-arg source is a non-constant lvalue (global, stack
local, deref, struct field, array element), BCC pushes both halves
**memory-direct** via `FF /6` Grp5 push variants — *not* through an
AX/DX intermediate. Same "high pushed first" rule still holds.
Encodings:

```
FF 36 lo hi    push word ptr DGROUP:_g          ; global (+FIXUPP)
FF 76 dd       push word ptr [bp+disp8]         ; stack local
FF 34          push word ptr [si]               ; ptr deref low (disp-less)
FF 74 dd       push word ptr [si+disp8]         ; ptr deref high
```

The disp-less low-half shortcut (`FF 34`, 2 bytes vs. `FF 74 00` at
3) is the same encoding choice BCC makes for the matching long-
pointer **load** (`mov dx, word ptr [si]` = `8B 14`). Two memory-
direct pushes per long arg, no register materialization step —
distinct from compilers that would `mov ax, [g+2] / push ax / mov
ax, [g] / push ax`. _Fixtures_: 322 (global), 323 (stack local),
325 (ptr deref), 326 (struct field), 328 (array element).

### Mixed int+long arg list pushed per-type, cleanup by byte-count (STRONG)

An argument list with both `int` and `long` parameters pushes
right-to-left (era-standard), but each arg uses its own type's push
shape: an `int` arg materializes into AX and emits `push ax`, while
a `long` arg uses the two-push memory-direct (or AX/DX) shape.
Cleanup is driven purely by the total pushed byte count, agnostic
to type — same threshold as the pure-int rule (≤4 bytes → `pop cx`
per word, ≥6 → `add sp, N`). Fixture 327 (`int a, long b`): 3 words
pushed = 6 bytes, cleanup is `add sp, 6`.

This is mostly just consistency with the existing cleanup rule, but
worth noting as the byte-count framing makes the rule simpler than
"per-arg" would be — and a compiler that special-cased mixed lists
would be distinguishable.

### Long-array global var-index: `<sym>[bx+disp]` not stack-array shape (STRONG)

Variable-indexed access to a `long` element of a **global** array
emits a 4-instruction sequence that's distinct from BCC's 5-
instruction stack-array effective-address pattern:

```
mov bx, <index>              ; or mov bx, [bp-N] / mov bx, <reg>
shl bx, 1
shl bx, 1                    ; bx = i * 4 (long stride)
mov ax, word ptr DGROUP:_a[bx+2]    ; 8B 87 lo hi (+FIXUPP)
mov dx, word ptr DGROUP:_a[bx]      ; 8B 97 lo hi (+FIXUPP)
```

The `<sym>[bx+disp]` form (`mod=10 r/m=111` ModR/M) folds the symbol's
segment-relative location into the disp16 slot at link time — there
is **no** runtime `lea ax, word ptr [bp-N] / add bx, ax`. Stack
arrays can't use this shape (no symbol to fold), so they pay the
extra two instructions. _Fixture_: 303 vs 079; 305 (write) vs 184.

### Stride is two `shl bx, 1`s, not `shl bx, 2` (STRONG)

Even though BCC could fold `× 4` into one `shl bx, 2` (80186+, two
bytes vs. four), it consistently emits `shl bx, 1 / shl bx, 1` for
long-stride indexing — same 8086-compatibility choice that gives
`inc ax / inc ax` for `r + 2` and avoids `imul reg, imm`. Same rule
across stack-int and register-int index loads. _Fixture_: 303, 305,
307.

### Long-array element layout (STRONG)

A `long a[N]` global has 4 bytes per element packed contiguously,
each element little-endian: low word at element-relative offset 0,
high word at offset 2. So `a[1]` of a long global at base 0 sits at
bytes 4–5 (low) and 6–7 (high). An initialized `long a[3] = {1, 2,
3}` emits **three 4-byte LE runs of `db`** — `db 1 / db 0 / db 0 /
db 0 / db 2 / db 0 / db 0 / db 0 / db 3 / db 0 / db 0 / db 0`. The
per-byte `db` (rather than `dw 1 / dw 0` per element) is the same
shape BCC uses for initialized int globals and switch linear-search
value tables — a consistent BCC idiom. _Fixtures_: 300, 301.

### Index-into-BX picks the shortest form (STRONG)

For variable-indexed array access (global or stack), BCC chooses
between three BX loads based on where the index sits:

- **Int stack local** — `mov bx, word ptr [bp-N]` (3 bytes).
- **Int register local** — `mov bx, <reg>` (2 bytes). Most
  common in loops where the index is the register-allocated loop
  variable.
- **Any other expression** — `<compute to AX> / mov bx, ax`.

Never `lea bx, [<bp+N>]` directly. The asymmetry mirrors the
"AX is the working register" pattern that shows up for `&x` and
compound `*=`. _Fixtures_: 303 (stack), 307 (register).

### Long-pointer deref: ABI registers, disp-less low load (STRONG)

A bare long-pointer deref (`g = *p` for `p: long *` in a register)
reads using the **ABI** register pair (DX=high, AX=low), *not* the
globals-arithmetic pair (AX=high, DX=low) that scalar long-global
arithmetic uses. The high half loads with disp8 (`8B 44 02`); the
low half uses the **disp-less** ModR/M form (`8B 14`):

```
mov ax, word ptr [si+2]    ; 8B 44 02   high
mov dx, word ptr [si]      ; 8B 14      low (no disp byte)
```

Two distinct ModR/M shapes for the same `[si+0]` semantic — BCC
picks the 1-byte-shorter `mod=00 r/m=100` encoding when disp=0.
The high-first load order still holds. _Fixture_: 309.

### Long-pointer compound assign: same `83`/`81` byte-width rule (STRONG)

`*p += K` / `*p &= K` for a register-resident `long *` uses the same
op-family imm width selection as long globals and long stack locals.
Only the ModR/M `r/m` field changes:

- Globals: `r/m=110` + disp16  → `83 06 lo hi K` / `81 26 lo hi K K2`
- Stack:   `r/m=110` + disp8   → `83 46 dd K` / `81 66 dd K K2`
- Pointer: `r/m=100` (`[si]`)  → `83 04 K` (low) / `83 54 02 00`
  (`adc [si+2], 0` high partner)

The choice of `83` (imm8sx) for arith and `81` (imm16) for bitwise
is preserved across all three storage classes — a strong indicator
that BCC's emitter selects opcode by source op, not by target
addressing mode. _Fixture_: 311 (arith `+=`).

### Array decay / `&<global>` skips AX round-trip (STRONG)

When the destination is a register, `&<global>` and array decay
both materialize as a direct `mov <reg>, offset DGROUP:_<sym>` —
no `lea ax, ... / mov <reg>, ax` pair. Same shortcut as for string
literals (fixture 088). Distinct from `&<stack-local>`, which
**always** routes through AX (`lea ax, word ptr [bp-N] / mov <reg>,
ax`) — stack addresses are runtime computations and need `lea`.

This split lets a disassembler recover the source storage class of
the address-of operand:

| Source form           | Asm                                       |
|-----------------------|-------------------------------------------|
| `r = &stack_var`      | `lea ax, [bp-N] / mov <r>, ax`            |
| `r = &global`         | `mov <r>, offset DGROUP:_<g>`             |
| `r = global_array`    | `mov <r>, offset DGROUP:_<a>`             |
| `r = "string literal"`| `mov <r>, offset DGROUP:s@`               |

The bottom three look mechanically identical but for the symbol
name, while the top has a distinctive `lea` prefix. _Fixtures_: 080
(stack `&x`), 308 (`&global`), 313 (array decay), 088 (string lit).

### Pointer ±K peephole crosses at stride 3 → `add reg, K` (STRONG)

The pointer increment / decrement peephole walks the same threshold
ladder as the int compound `x += K` peephole (see "`inc <reg>` /
`dec <reg>` for `±1`, doubled for `±2`"), but applied to the
*pointee size*:

| Pointer type    | Stride | `p++` emits      | Bytes |
|-----------------|--------|------------------|-------|
| `char *`        | 1      | `inc <reg>`      | 1     |
| `int *`         | 2      | `inc / inc`      | 2     |
| `long *`        | 4      | `add <reg>, 4`   | 3     |

So `long *p; p++;` emits `add si, 4` (3 bytes), not four `inc si`s
(which would cost 4). The crossover from `inc`-repeats to `add`
sits between stride 2 and stride 3 — same threshold as the int
compound peephole. _Fixtures_: 093 (char*), 090 (int*), 313 (long*).

### Every long memory store: high half first, then low (STRONG)

A consistent invariant across every long memory write BCC emits:
**the high half is stored before the low half**, regardless of
storage class or operand shape. The same rule applies whether the
value being stored is a constant, a register pair, or part of a
read-modify-write chain.

| Target                            | High store              | Low store               |
|-----------------------------------|-------------------------|-------------------------|
| Long global (const)               | `mov [_g+2], hi`        | `mov [_g], lo`          |
| Long global (DX:AX from call)     | `mov [_g+2], dx`        | `mov [_g], ax`          |
| Long global (AX:DX from arith)    | `mov [_g+2], ax`        | `mov [_g], dx`          |
| Long stack local (const)          | `mov [bp+off+2], hi`    | `mov [bp+off], lo`      |
| Long stack local (DX:AX from call)| `mov [bp+off+2], dx`    | `mov [bp+off], ax`      |
| Long pointer deref (`*p = K`)     | `mov [si+2], hi`        | `mov [si], lo`          |
| Long struct field                 | `mov [_s+off+2], hi`    | `mov [_s+off], lo`      |
| Long array element (const idx)    | `mov [_a+off+2], hi`    | `mov [_a+off], lo`      |
| Long array element (var idx)      | `mov [_a+bx+2], hi`     | `mov [_a+bx], lo`       |

Note that the **load** order also follows the same "high first" rule
when reading a long into a register pair (whether AX:DX globals-
convention or DX:AX ABI-convention). So a long-to-long copy is
always: load high, load low, store high, store low. _Fixtures_: 207,
286, 308, 316, 318, 304, 302, 305, 314, 315.

### Long function-call return: DX:AX ABI, stored high-first (STRONG)

A function with `long` return value returns it in `DX:AX` (high:low
— standard 16-bit cdecl ABI). The caller-side store directly writes
both halves with `mov reg → mem` encodings (`89 56`/`89 46` for
bp-relative stack stores, `A3`/`89 16` for moffs16 global stores)
in **DX first, then AX** order — never the opposite. Distinguishable
from a compiler that would route long return values through a stack
spill before storing.

_Fixtures_: 314 (global), 315 (long local init), 321 (long local
assign).

### Long struct fields: tight packing, +0/+2 element layout (STRONG)

A struct containing `long` fields packs them with no padding —
following BCC's general struct packing rule (already documented).
For a long field at struct-relative offset N:

- The low word lives at `<base>+N`
- The high word lives at `<base>+N+2`

The base depends on storage class: `DGROUP:_s` for a global struct,
`[bp+struct_off]` for a stack struct, `[<reg>]` for a struct
pointer's pointee. Reads and writes both use the same memory-direct
two-word access pattern as any other long lvalue. `p->x` for a long
field at offset 0 is byte-identical to `*p` for a long pointer.

The lack of alignment padding makes `struct { int a; long x; }`
land `x` at byte offset 2 — *not* the +4 a strict-alignment
compiler would pick. The long's low half is on an even-aligned word
(byte 2), but the natural-alignment slot for a 32-bit value would be
byte 4. BCC accepts the misaligned access; the 8086 handles it
transparently. _Fixtures_: 316–320.

### Long-neg idiom: `neg ax / neg dx / sbb ax, 0` (DEFINITIVE)

`-x` for a 32-bit `long` lowers to a specific 3-instruction sequence
once AX:DX hold the operand (AX=high, DX=low, globals convention):

```
neg ax                ; F7 D8 — negate high half (CF effect discarded)
neg dx                ; F7 DA — negate low; CF=1 iff low was non-zero
sbb ax, 0             ; 1D 00 00 — propagate borrow into high (AX -= CF)
```

The `neg ax` step comes **first** even though its CF setting gets
clobbered by the subsequent `neg dx`. The order is non-obvious —
the "natural" pattern would be `neg DX / sbb AX, 0 / neg AX` (low,
propagate, high) — but BCC's ordering still produces the correct
two's-complement result because the final value of CF (after `neg
dx`) reflects whether the original low was non-zero, and `sbb ax, 0`
correctly applies that borrow.

The sequence is short enough that a coincidental match by another
compiler is implausible: three specific opcodes in a specific order
operating on the AX/DX pair. Combined with the surrounding load/
store frame around AX and DX, finding `F7 D8 / F7 DA / 1D 00 00`
adjacent in a binary is essentially conclusive proof of BCC long
codegen. _Fixture_: 331.

### Long bitwise complement: `not dx / not ax` — low-first order (STRONG)

For `~x` on a long, BCC emits `not dx` then `not ax` — **low half
first, then high half**. This contrasts with BCC's high-first
ordering rule for *loads* (`mov ax, [hi] / mov dx, [lo]`) and
*memory stores* (`mov [hi], ax / mov [lo], dx`).

Since bitwise unary has no carry/borrow dependency, the order is
arbitrary for correctness. BCC's choice is consistent: bitwise
unary uses low-first, arithmetic load/store uses high-first. A
compiler that picked high-first for `not` (or low-first for the
arithmetic load) would be distinguishable. _Fixture_: 332.

### Long stack-local arithmetic register convention is destination-driven (STRONG)

For long binary arithmetic between stack-local operands `x` and `y`
whose result `z` is also stack-resident, BCC picks AX=high / DX=low
(the **globals-arithmetic convention**), not DX=high / AX=low (the
ABI convention). The choice is driven by where the result goes:

- Result → memory (global, stack, struct field, pointer-target) →
  AX=high, DX=low. The natural `mov [hi], ax / mov [lo], dx` store
  drops out without a register swap.
- Result → return (`return a+b;` in a long-returning function) →
  DX=high, AX=low. Matches the cdecl long ABI.
- Result → helper call (`*= / /=` etc.) → DX=high, AX=low. The
  runtime helpers take and return DX:AX = high:low.

So one disassembler heuristic for spotting BCC long arithmetic
that's about to be stored is: AX:DX load order with AX getting the
higher address. Distinct from the ABI pair which loads DX from the
higher address. _Fixtures_: 329, 330, 333, 334 (memory-bound, AX=hi)
vs. 285 (return, DX=hi).

### Compound assign opcode tells constant-vs-variable RHS (STRONG)

For long compound assigns to memory, the **opcode byte** of the
read-modify-write differs depending on whether the source RHS was
a constant or a variable. Both shapes use ModR/M `r/m=110` (mem-
relative addressing); the opcode distinguishes them:

| Compound | Constant RHS opcode      | Variable RHS opcode        |
|----------|--------------------------|----------------------------|
| `x +=`   | `83 /0` (low) + `83 /2` (adc, high) | `01` (low, src=DX) + `11` (adc, high, src=AX) |
| `x -=`   | `83 /5` (low) + `83 /3` (sbb, high) | `29` (low, src=DX) + `19` (sbb, high, src=AX) |
| `x &=`   | `81 /4`                  | (no fixture yet — likely `21`) |
| `x \|=`  | `81 /1`                  | (no fixture yet — likely `09`) |
| `x ^=`   | `81 /6`                  | (no fixture yet — likely `31`) |

Constant-RHS uses `Grp1 r/m16, imm8sx` (opcode `83`) or `Grp1
r/m16, imm16` (opcode `81`). Variable-RHS uses the simpler
`<op> r/m16, r16` opcodes (`01`/`11`/`29`/`19` and bitwise
siblings).

A disassembler can recover whether the source RHS was a literal
*or* a named variable just by inspecting the opcode byte —
without needing to see the prior load that brought the variable
into AX:DX. The two byte sequences are distinct shapes for what
the C language treats as the same operator. _Fixtures_: 288, 251
(constant arith); 339, 340 (variable arith); 289 (constant bitwise).

### Long stack-local mul/div/mod: helper shape inherited from globals (STRONG)

The long multiply/divide/modulo helpers (`N_LXMUL@`, `N_LDIV@`,
`N_LMOD@`, `N_LUDIV@`, `N_LUMOD@`) have a fixed calling convention
that BCC obeys whether the operands are globals or stack locals:

- **Multiply**: operand A in `CX:BX` (high:low), operand B in
  `DX:AX` (high:low). Result in `DX:AX`. _Not_ stack-passed.
- **Divide/Modulo**: 4-word stack push, **divisor first** (high
  then low), **dividend second** (high then low). Result in
  `DX:AX`. Helper pops its own 8 args (no caller cleanup).

The operand-A→CX:BX choice for multiply is distinctive — most
compilers would route both operands through the same register
pair. The divide convention's per-operand high-first push order
matches BCC's general long-arg convention. _Fixtures_: 231 (global
mul) / 336 (stack mul); 232 (global div) / 337 (stack div).

### Variable shift count loaded as `byte ptr` (STRONG)

For both `int` and `long` shifts by a variable count, BCC loads
the shift count into `CL` using a **byte ptr** load — never a
`word ptr` load followed by a register truncation:

```
mov cl, byte ptr [bp+n_off]    ; int n = 3; shift count load
```

The high byte of `n` is never read; the helper / `shl` only
consumes CL. This mirrors the int shift's documented behavior
(fixtures 017/018) and extends to long shift via helper (fixture
341). _Fixtures_: 017, 018 (int), 341 (long).

---

## Calling-convention signatures

### Args pushed right-to-left, caller cleans (WEAK)

Standard cdecl convention. Era-universal.

### `call near ptr _<name>` with explicit `near ptr` (STRONG, .asm-specific)

The `near ptr` qualifier is technically redundant in the small memory
model — the assembler can infer it. BCC always writes it explicitly.
Other assemblers' emitters may omit. _Fixture_: 010.

### Arg cleanup: `pop cx` per arg ≤2, `add sp, N*2` for ≥3 (STRONG)

The threshold is byte-cost driven: 1-2 pops fit in 1-2 bytes; 3+
pops cost 3+ bytes while `add sp, N*2` costs just 3. BCC picks the
shorter form at the 3-arg boundary. _Fixtures_: 033 (1 arg), 034 (2),
049 (3).

### `mov ax, K / push ax` for constant int arg (STRONG)

The 80186+ `push imm` is avoided. BCC always materializes the arg in
AX first. _Fixture_: 033.

### `mov al, K / push ax` for constant char arg (STRONG)

Char args use the 8-bit form to set AL, then push the full word. The
high byte is undefined garbage. _Fixture_: 052.

### Char param read from byte slot (STRONG)

`mov <char-reg>, byte ptr [bp+N]` — the callee reads only the low
byte. _Fixture_: 052.

### Params: leftmost at `[bp+4]` (small model) (STRONG)

After `push bp / mov bp, sp`, the first arg sits at `[bp+4]` (2 bytes
saved `bp` + 2 bytes near-call return address). Each subsequent arg
adds **the previous arg's width** — 2 for `int`/`char`/pointer, 4 for
`long`/`unsigned long`. Distinctive vs medium/large models (`[bp+6]`,
far call). _Fixtures_: 033 (all ints), 285 (`long a, long b` lands b's
low half at `[bp+8]`, not `[bp+6]`).

---

## Control-flow shape signatures

### `if`-`else` always emits the jump-over-else, even when dead (DEFINITIVE)

```
	<cond>
	j<inv>	short @else
	<then>
	jmp	short @end       ; <— always emitted
@else:
	<else>
@end:
```

The `jmp short @end` between then-branch and `@else:` is emitted
**even when the then-branch ends with a `return`** (making the jmp
unreachable). Almost every other compiler does dead-code elimination
on this. _Fixture_: 026.

### `while` trampoline `jmp short check` at the top (STRONG)

BCC emits `jmp short @check` before the body label, even when the
condition is trivially `true`. Other compilers may invert the check
and put it at the top. _Fixture_: 027.

### `for` step inlined into body (STRONG)

BCC's `for (init; cond; step) body` emits the step in the same block
as the body, with no separate `@step` label unless `continue` is
present. Some compilers always label the step. _Fixture_: 061.

### `do-while`: no trampoline, body label at top (STRONG)

```
@body:
	<body>
	<cond/j-true body>
```

Cleaner than the `while` shape; the body always runs at least once,
so no jump to the check is needed. Compilers that translate
`do-while` into a generic loop with a flag would be distinguishable.
_Fixture_: 062.

### `break-target` label suppressed when unused (STRONG)

The loop's break-target slot is always **reserved** in the label
numbering, but the label itself is only **emitted** when the body
actually contains a `break;`. The reservation can leave a "hole" in
the slot numbering (e.g., 027 reserves slot 2 but never emits it —
slot 3 is exit), which is itself a fingerprint. _Fixtures_: 027 vs 063.

### `&&` / `||` short-circuit shapes (DEFINITIVE)

Distinctive 4-slot expression-position layout:

```
<cmp / j-false-mat>      ; for each operand of &&
<cmp / j-false-mat>
mov ax, 1
jmp short @end
@false-mat:
xor ax, ax
@end:
```

The exact slot reservation (4 slots even for `&&`, with two unused)
matches BCC's plan. The branching code shape for `if (a && b)` —
all operands targeting the same skip label, no consolidating label —
is also distinctive. _Fixtures_: 056–060.

### Comparison-as-value pattern (DEFINITIVE)

```
mov ax, <left>
cmp ax, <right>
j<inv> short @false
mov ax, 1
jmp short @end
@false:
xor ax, ax
@end:
```

Always 6 instructions. Many later compilers use `setcc` (80386+) or
omit the `mov ax, 1 / jmp` when the value isn't actually needed. BCC's
fixed pattern is a strong tell. _Fixture_: 019.

### Always signed-jump mnemonics for `int` comparisons (STRONG)

BCC uses `jl/jg/jle/jge` not the unsigned variants `jb/ja/jbe/jae`,
even when both operands are non-negative. Reflects the signed default
for C `int`. _Fixtures_: 019–024.

### Packed structs with no inter-field padding (STRONG)

BCC packs struct fields tightly: `{char c; int n;}` puts `c` at
offset 0 and `n` at offset **1** (not 2 as most C compilers
would for alignment). The int is therefore misaligned on the
8086, but the chip handles it transparently. The total struct
size rounds up to an even multiple of 2. Many later DOS
compilers (MSC 5.x+, Watcom) pad to align — this is a distinctive
BCC choice. _Fixture_: 102.

### `extrn _<name>:near` between `_TEXT ends` and publics (STRONG)

Calls to functions not defined in this TU produce one
`extrn _<name>:near` directive each, emitted in the file tail
*before* the `public` list. The `:near` suffix is BCC-specific
(MASM prefers `extern <name>:PROC`). _Fixtures_: 096–100.

### Non-printable bytes break `db` quote runs (STRONG)

A string literal containing `\n` (byte 10) splits into:
```
db	'hi'
db	10
db	0
```
The quoted form only holds printable ASCII; control bytes get
their own decimal `db <N>` lines. Most era compilers either
escape (`\n` in a quoted string) or emit one big `dw`/`db` list.
_Fixture_: 098.

### Each string literal explicitly NUL-terminated (STRONG)

Every literal in `s@` ends with an explicit `db 0` — the NUL isn't
embedded inside the quoted form. Multiple literals stack with
their NULs visible:
```
db	'a' / db 0 / db 'b' / db 0
```
_Fixtures_: 088, 098, 100.

### `switch` strategy fingerprint: three observable forms (DEFINITIVE)

BCC picks one of exactly three dispatch shapes — the choice is a
combined function of case count and density, and each shape has
distinctive byte-level features:

- **Chained compares (`cmp + je` chain ending in `jmp`)** for 3 or
  fewer non-default cases. Triggers the `or ax,ax` peephole if any
  case has value 0. _Fixtures_: 072, 075.
- **Jump table** (`cmp / ja / shl / jmp word ptr cs:@<fn>@C<n>[bx]`)
  for ≥ 4 contiguous cases starting at 0. Address table emitted
  AFTER `_main endp` as `dw @<fn>@<slot>` entries under a `@<fn>@C<n>
  label word` header. _Fixtures_: 073, 076.
- **Linear-search loop** (`mov cx, N / mov bx, offset C<n> / loop
  with je dispatch / jmp word ptr cs:[bx+OFF]`) for ≥ 4 sparse cases.
  Two parallel tables (values then addresses), with **values
  written as `db` byte pairs** in little-endian order (e.g. `1000`
  → `db 232 / db 3`). Adds a hidden 2-byte stack slot (visible as
  the prologue using `sub sp, N` with N including 2 extra bytes
  beyond what user locals need). _Fixture_: 074.

The presence of a `@<fn>@C<num>` label between `_main endp` and
`?debug C E9` is a strong stand-alone tell — most compilers emit
jump tables in `_DATA` or inline.

The `<num>` itself is a deterministic-but-unexplained quantity:
fixtures 073 (jump-table n=8, `C1244`) and 076 (jump-table n=4,
`C876`) fit `92·n + 508`, and 074 (linear n=4, `C738`) fits
`74·n + 442`. Whether these constants depend on anything else
(function position, TU contents, surrounding constants) is an
open question — see specs/bcc/ASM_OUTPUT.md.

### Variable array indexing: 5-instruction effective-address (STRONG)

For `a[i]` with non-constant `i`, BCC always emits this exact
sequence — both for reads and writes:

```
mov bx, <i>
shl bx, 1                ; only for int arrays (stride 2)
lea ax, word ptr [bp-N]  ; note the `word ptr` on lea
add bx, ax
mov <width> ptr [bx], <rhs>   ; or mov ax, <width> ptr [bx]
```

Distinctive features:
- `lea ax, word ptr [bp-N]` with the redundant `word ptr` annotation
- Address goes through AX rather than `lea bx, ...` directly
- The `add bx, ax` over `lea bx, [bx+ax]` (which exists)

Other era compilers compute via `lea bx, [bp+si-N]` (one
instruction) or fold via SI. BCC's 4-instruction address compute is
characteristic. _Fixture_: 079.

### `&x` via `lea ax / mov <dst>, ax` (STRONG)

Even when the destination is a register, address-of materializes
into AX first and then `mov`s to the destination — never `lea
<dst>, [bp-N]` directly. Consistent with BCC's "AX is the
working register" pattern. _Fixtures_: 080, 081.

### `DGROUP:` segment override on every global access (STRONG)

Every read or write of a file-scope variable carries the explicit
`DGROUP:` prefix: `mov ax, word ptr DGROUP:_g`. Many era compilers
omitted the override and relied on the default `DS:`; BCC's
verbose form is distinctive. _Fixtures_: 083–087.

### Initialized data emits as `db` byte pairs (STRONG)

An `int g = 42` global emits `db 42 / db 0` rather than `dw 42`.
Same byte-pair shape as the linear-search switch value table — a
consistent BCC idiom for emitting 16-bit values into data
segments. _Fixtures_: 084, 086, 087.

### `?debug C E9` moves when BSS is non-empty (DEFINITIVE)

The end-of-function debug record `?debug C E9` normally sits
inside `_TEXT` between `_main endp` and `_TEXT ends`. **When the
program has any uninitialized globals, the record moves to inside
the trailing `_BSS` block**, right before `_BSS ends`. The shift
is structural and reliable. _Fixtures_: 083, 085, 087.

### Char-on-right widening dance (STRONG)

When a char operand appears as the right side of arithmetic with
an int LHS, BCC emits the six-instruction sequence
`push ax / mov al, ... / cbw / mov dx, ax / pop ax / add ax, dx`
to preserve the running int sum through the char load.  Most
era compilers either widen the char eagerly before the int load,
or use a different temp register without the push/pop. _Fixture_:
087.

### String literals materialize after `s@` (DEFINITIVE)

The `s@ label byte` marker in the trailing `_DATA` block is the
anchor for string literals. Each literal becomes `db '<chars>'`
followed by an explicit `db 0` NUL terminator. References to the
first literal use `offset DGROUP:s@`; subsequent literals would
use `s@+<offset>`. The presence of `s@` in any BCC-emitted `.ASM`
is essentially conclusive. _Fixtures_: 088, 089.

### Direct `mov reg, offset DGROUP:s@` for string-literal addresses (STRONG)

Unlike `&x` (which always routes through AX), assigning a string
literal address to a register uses a direct `mov` with an
`offset` immediate: `mov si, offset DGROUP:s@`. The distinction
is mechanical (literals are linker-resolved constants vs. `&x`
which is a runtime `lea`), but few compilers special-case it as
visibly. _Fixture_: 088.

### Pointer direct-deref bonus (STRONG, refined)

Pointer locals share the int `≥ 3 uses` enregistration threshold,
but **direct dereferences contribute 2 uses each** to the pointer
name. "Direct" means `*p`, `p[i]`, or `*(p + <constant>)` —
syntactic forms BCC can fold into a single addressed-load
idiom. A *variable* offset (`*(p + i)`) doesn't get the bonus,
because the address arithmetic uses a temp register anyway.

The earlier slice documented this as "pointer threshold is 2",
which was an artifact of having only `*p`-style fixtures (where
the bonus made the total exactly 3). Fixture 092 disambiguated:
`int *p = a; ... return *(p + i);` has p with only `1 + 1 = 2`
uses (no bonus, because the offset is variable), and p stays on
the stack — confirming the int threshold rather than a relaxed
pointer-specific one.

_Fixtures_: 080, 081, 088, 091 (bonus applies → enregister);
092 (no bonus → stack).

### Array name decays to `lea ax, [bp-N]` (STRONG)

Using an array name in a non-index expression context (`int *p = a;`,
`f(a)`, etc.) emits the same effective-address load as `&a[0]`:
`lea ax, word ptr [bp-N]`. The `word ptr` annotation on `lea`
(which doesn't actually load data) is a BCC tic. _Fixtures_: 090, 095.

### `p++` emits `stride` × inc/dec, not `add reg, K` (STRONG)

For `int *p; p++;`, BCC emits **two** `inc <reg>` instructions
(2 bytes total) rather than `add <reg>, 2` (3 bytes). For
`char *s; s++;`, it's a single `inc`. The pattern continues from
the `±2` int peephole in regular assignments. _Fixtures_: 090, 093.

### `*(p + K)` folds to indexed addressing (STRONG)

A constant-offset deref like `*(p + 1)` becomes `[reg + K*stride]`
in one instruction (`mov ax, word ptr [si+2]`). Both source forms
`*(p + 1)` and `p[1]` produce the *same* asm — BCC sees them as
equivalent at the AST level. _Fixtures_: 091, 094.

### `switch` slot-reservation fingerprint (STRONG)

The slot counter advances past unused "ghost" slots before the
first case body:
- Chained / linear-search: `#non-default-cases + 2` ghost slots.
- Jump-table: `3` ghost slots.

Visible as label-number gaps: 072's first case label is `@1@170`
(slot 5) rather than `@1@50`. _Fixtures_: 072–076.

---

## Source-line comment signatures (.asm-specific)

The interleaved `;\t<source>\r\n` comments are an `-S` artifact;
they don't survive to `.obj` or `.exe`. But for `.asm` recognition:

### Three-line comment blocks (DEFINITIVE)

Each block is `<blank-comment><source><blank-comment>` — never just
the source line. Distinctive vs MSC's `; <source>` single-line form.

### Multi-line blocks when no asm between source lines (STRONG)

When BCC's emission skips multiple source lines without producing any
asm (e.g., the `for` header followed immediately by the body's first
statement), all skipped lines fold into one comment block. The
"missing" blank-comment between them is the tell. _Fixture_: 027.

### First comment block skips prior lines (STRONG)

The very first comment block in a function does *not* include source
lines from before the function. This is important for multi-function
TUs. _Fixture_: 009.

---

## Ambiguous patterns (decompilation signals worth noting)

These patterns are clear *signatures of BCC* (they help fingerprint),
but they are *ambiguous as decompilation evidence* — multiple distinct
C sources collapse to the same asm:

- **`xor ax, ax`** could be `return 0;` OR `int x = 0;` initialization
  OR a comparison-as-value's false-branch.
- **`mov ax, K / jmp <exit>`** could be `return K;` OR `int x = K;
  return x;`.
- **`inc <reg>`** as a standalone statement could be `++x;`, `x++;`,
  `x += 1;`. The pre/post distinction is **lost** when the value is
  discarded.
- **`add <reg>, K`** as a standalone statement could be `x += K;` —
  but `x = x + K;` produces different asm, so this *is* recoverable.
- **`mov ax, <reg> / inc ax`** at end of expression could be either
  `return ++x` (pre-form) or — no, wait, post-form is `mov ax,<reg> /
  inc <reg>`, different. So this pre/post distinction IS recoverable.

For a decompiler, the asm-to-source map is a one-to-many in the
ambiguous cases. A decompiler that aims at "byte-exact resynthesis"
needs to pick the *canonical* source form (probably the simplest /
most idiomatic), which is a separate design choice from compiler
identification.

---

## What we don't yet know (open fingerprint questions)

- **`-O` switch differences**: We've only captured at default
  optimization. Higher `-O` levels may change every peephole.
- **Other memory models** (`-mc`, `-ml`, `-mt`, `-mh`): Far calls,
  different param offsets, different segment scaffolds.
- **BCC 2.0 vs 3.0 vs other Turbo C++ versions**: Most patterns
  likely identical (the calling convention is fixed), but specific
  peepholes may have evolved.
- **Pre-built libraries**: `LIB_ARCHIVE.md` shows the runtime archives
  mix BCC-style and assembler-style members, and TLIB strips the direct
  BCC translator COMENTs. We still need a fuller member-classification
  pass and dictionary decoding.
- **`.obj` vs `.exe`**: OMF record ordering/fixup encoding now provides
  object-level fingerprints. TLINK's MZ executable layout is still
  largely unfixtured.

## What we want next for the fingerprinter

`crates/fingerprint/` already performs basic OBJ/LIB analysis. Useful
next steps are:

1. **A pattern database** parseable from this doc or generated from
   fixtures directly).
2. **Disassembler integration** — needs to read OMF `.obj`, MZ `.exe`,
   and probably PE `.exe` (later compilers).
3. **Probabilistic scoring** — weighted evidence combination, with
   per-pattern strength tuned against a corpus of known-compiler
   binaries.
4. **Cross-compiler comparison** — once we have a second compiler's
   patterns (MSC 6, Turbo C 2, etc.), the discriminator can pick the
   best match rather than just a binary yes/no.
