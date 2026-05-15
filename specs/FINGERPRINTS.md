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

## How to use this catalog (when the fingerprinter exists)

A future fingerprint tool would:

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

### `public` symbols in reverse-alphabetical order (DEFINITIVE)

```
	public	_main
	public	_f
```

The `public` list at end-of-file is sorted **reverse-alphabetical**
by symbol name (both functions and globals participate in one
combined sort). Initial fixtures (009, 010, 087) seemed to suggest
"reverse declaration order" because source order happened to match
alpha order; fixture 095 (`sum` defined first, `main` second,
emitted as `sum, main`) disambiguates — that's the reverse-alpha
walk (`sum > main`), not reverse-source. Most likely internal
implementation: a sorted symbol table walked in reverse at TU end.

Almost no other compiler matches this. _Fixtures_: 009, 087, 095.

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

Every `return` becomes `jmp short @<n>@50` to a single exit label,
where the epilogue lives. Even an unconditional `return 0;` in a
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
combines with the reverse-alphabetical public-symbol ordering as a
cross-check.

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

### Long compound assigns: arith uses imm8sx, bitwise uses imm16 (STRONG)

For 32-bit `long` globals, BCC emits a memory-direct read-modify-write
pair, **but the immediate encoding width is op-family dependent**:

- `+=` / `-=` use `83 06 ...` / `83 2E ...` — Grp1 `r/m16, imm8sx`,
  **5 bytes** per half. The high partner is always `adc 0` / `sbb 0`
  (also 5 bytes each).
- `&=` / `|=` / `^=` use `81 26 ...` / `81 0E ...` / `81 36 ...` —
  Grp1 `r/m16, imm16`, **6 bytes** per half, **even when the
  immediate trivially fits in an i8sx**.

Concrete: `long g; g &= 15;` emits
```
and word ptr DGROUP:_g, 15        ; 81 26 lo hi 0F 00 (6 bytes)
and word ptr DGROUP:_g+2, 0       ; 81 26 lo hi 00 00 (6 bytes)
```
The 4-byte saving from picking `83 26 lo hi 0F` (5 bytes) is left on
the table. This isn't a TASM default — TASM's sign-extension heuristic
would pick the shorter form for `15` if BCC asked for it. BCC's
emission must specifically choose `81` for the bitwise compound
shapes, while picking `83` for the arithmetic siblings. Distinct from
slice-207-style `g = g <op> K` which routes through registers
entirely. _Fixtures_: 251 (`+=` → 83), 253 (`&=` → 81).

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
adds 2 to the offset. Distinctive vs medium/large models (`[bp+6]`,
far call). _Fixture_: 033.

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
- **Pre-built libraries**: Functions linked from the runtime would
  have their own fingerprint. May actually be a *stronger* signal
  than user code since libraries are deterministic.
- **`.obj`-only vs `.exe`**: The OMF record encoding (when we build
  the OBJ emitter) will introduce a whole new layer of fingerprints
  (record-type ordering, fixup encoding, segment alignment).

## What we want for the eventual fingerprinter

When we build the recognizer tool (probably as a separate crate):

1. **A pattern database** parseable from this doc (or generated from
   fixtures directly).
2. **Disassembler integration** — needs to read OMF `.obj`, MZ `.exe`,
   and probably PE `.exe` (later compilers).
3. **Probabilistic scoring** — weighted evidence combination, with
   per-pattern strength tuned against a corpus of known-compiler
   binaries.
4. **Cross-compiler comparison** — once we have a second compiler's
   patterns (MSC 6, Turbo C 2, etc.), the discriminator can pick the
   best match rather than just a binary yes/no.
