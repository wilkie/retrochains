# BCC `-S` assembly output format

What `BCC.EXE -S <source>.C` writes to `<source>.ASM`. This document
started from the first `-S` fixtures and has been extended as the
fixture corpus grew. When a rule is not closed over the full corpus,
the text should say which fixtures pin it and which cases remain open.

## File-level conventions

- **Line endings: CRLF.** Every line ends with `0x0D 0x0A`.
- **EOF marker.** The file ends with a `0x1A` byte (DOS Ctrl-Z), classic
  DOS text-file termination.
- **Indentation: TAB (`0x09`).** Single tab, not spaces.
- **Filename in the file content is lowercased.** We pass `HELLO.C` on the
  command line; the debug record and source comments refer to `hello.c`.
  BCC lowercases the basename when stamping it into the output.
- **Output filename matches input basename, uppercased, `.ASM`.** Input
  `HELLO.C` → output `HELLO.ASM`.

## Skeleton

Every `.ASM` file starts with the same fixed preamble of macro definitions
and ends with the same fixed tail. Everything that *varies* between
fixtures lives in two places:

1. The two `?debug` records near the top (filename + DOS-packed mtime).
2. The function body itself.

### Macro preamble (lines 1–14, byte-for-byte identical across fixtures)

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

`??version` is a symbol defined by recent TASMs; the `ifndef` arm makes
the file work both when assembled standalone and when assembled by a TASM
that already provides debug macros.

### Debug header (parameterized)

```
	?debug	S "hello.c"
	?debug	C E9006097160768656C6C6F2E63
```

- `?debug S` — source filename (lowercased), quoted.
- `?debug C` — comment record, hex-encoded. Layout of the bytes:
  - `E9` — record subtype tag.
  - 4 bytes — DOS-packed mtime of the source file (little-endian).
    In the fixture: `00 60 97 16` = `0x16970060` = 1991-04-23 12:00:00,
    matching the oracle's pin (so BCC is reading the source's *mtime via
    DOS stat*, not the host clock or the embedded `?debug S` filename).
  - 1 byte — filename length.
  - N bytes — filename (lowercased, ASCII).

### Segment scaffold (constant across fixtures)

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

The `_TEXT` is opened-and-immediately-closed, then `DGROUP` is declared,
then `_DATA` and `_BSS` are declared with the `d@`/`d@w` and `b@`/`b@w`
byte/word labels respectively. These labels appear to be section-base
markers BCC uses internally.

### Function emission

`_TEXT segment byte public 'CODE'` is opened **once per translation
unit**, not per function. All functions in a translation unit live
inside one `_TEXT` segment. After the last function, a single
`?debug C E9` record marks end-of-translation-unit-code, and then
`_TEXT ends` closes the segment.

```
_TEXT	segment byte public 'CODE'           ;  opens once
   ;	
   ;	int f(void) { return 1; }
   ;	
	assume	cs:_TEXT
_f	proc	near
	... body, single exit label @1@50 ...
_f	endp
   ;	
   ;	int main(void) { return 0; }
   ;	
	assume	cs:_TEXT
_main	proc	near
	... body, single exit label @2@50 ...
_main	endp
	?debug	C E9                              ; once, at the end
_TEXT	ends                                  ; closes once
```

- Source-line comments are emitted as `   ;\t<source-text>\r\n`, with each
  statement bracketed by an empty `   ;\t` line before and after the next
  comment block.
- C function name `main` becomes ASM symbol `_main` (leading underscore is
  the standard Borland small-model convention).
- Every function has a **single exit label** `@<func-idx>@<label-idx>`.
  `func-idx` increments per function definition in source order (1, 2,
  3, ...). `label-idx` is `50` for the exit label (consistent across
  fixtures 001, 003, 004, 005, 006, 009, 010). Even an unconditional
  return goes via `jmp short @<f>@50` to that label, which holds the
  epilogue.
- A re-issued `\tassume\tcs:_TEXT\r\n` precedes every `proc near` —
  even for the *second* function in a TU, despite the `assume` from the
  segment scaffold above still being in scope. (Belt-and-suspenders for
  the linker / debugger.)

### Tail

```
_DATA	segment word public 'DATA'
s@	label	byte
_DATA	ends
_TEXT	segment byte public 'CODE'
_TEXT	ends
	public	_<sym>            ; one line per defined function
	...
	end
```

`s@` is another section-base label, this time for string-pool data.
It remains as an empty marker when a translation unit has no string
literals, and it fills with `db` runs when literals are present. The
`_TEXT` is re-opened and re-closed in case later sections need it.

The `public _<sym>` lines are **not source order**. Early fixtures 009
and 010 looked like reverse definition order because `f` and `main`
happened to fall that way, but later fixtures disprove that simple rule.
The current implementation uses a fixture-fitting length-bucket plus
reverse-alphabetical approximation; targeted probes suggest BCC's real
symbol table order is hash-bucket based. See the dedicated public-order
section below and `OPEN_QUESTIONS.md`.

`end` closes the assembly file (TASM's end-of-source directive).

## Codegen patterns observed

### Return constant

| C source       | Asm                | Notes                                  |
| -------------- | ------------------ | -------------------------------------- |
| `return 0;`    | `xor ax,ax`        | Smaller encoding than `mov ax,0`       |
| `return 42;`   | `mov ax,42`        | Decimal in source; no `0x`/`h` suffix  |

Both are followed by `jmp short @1@50` to the unified exit, never an
inline `ret`.

### Stack frame

- **No locals (`001`, `003`, `005`)**: `push bp` / `mov bp,sp` ...
  `pop bp` / `ret`.
- **One `int` local — 2 bytes (`004`)**: `push bp` / `mov bp,sp` /
  **`dec sp` / `dec sp`** ... `mov sp,bp` / `pop bp` / `ret`. The local
  is at `[bp-2]`.
- **Two `int` locals — 4 bytes (`006`)**: `push bp` / `mov bp,sp` /
  **`sub sp,4`** ... `mov sp,bp` / `pop bp` / `ret`. The locals are at
  `[bp-2]` and `[bp-4]`.

`dec sp` decrements SP by **one byte** (it's the single-byte register
DEC encoding). For 2 bytes of locals BCC emits two `dec sp` (2-byte
total prologue extension); for 4 bytes it switches to `sub sp,4` (3- or
4-byte instruction). The threshold sits between 2 and 4 bytes. The
exact crossover and any larger-frame patterns will be pinned down by
future fixtures.

### Local-variable access

```
	mov	word ptr [bp-2],5         ; int x = 5;
	mov	ax,word ptr [bp-2]        ; return x;
```

`word ptr` is always written explicitly even when the destination is a
16-bit register that would otherwise disambiguate the size.

Multiple `int` locals are laid out in declaration order with 2-byte
offsets growing toward more-negative offsets from `bp`:

```
	mov	word ptr [bp-2],5         ; int x = 5;
	mov	word ptr [bp-4],7         ; int y = 7;
```

### Binary `+` (`006`)

```
	mov	ax,word ptr [bp-2]        ; load left operand into AX
	add	ax,word ptr [bp-4]        ; add right operand straight from memory
```

The left operand is loaded into AX; the right operand is added in a
single memory-to-register `add`. No third register is involved for this
pattern.

### Binary `-` (`007`)

Symmetric to `+`, with `sub`:

```
	mov	ax,word ptr [bp-2]
	sub	ax,word ptr [bp-4]
```

### Binary `*` (`008`)

```
	mov	ax,word ptr [bp-2]
	imul	word ptr [bp-4]
```

This is the **single-operand IMUL** form: `imul src` ≡ `DX:AX ← AX *
src`. For 16-bit `int` we only need AX (DX is discarded). BCC picks this
over the 80186+ two-operand `imul reg, src` form even when targeting
small-model 16-bit code — presumably because that's the most-compatible
encoding (8086 supports it).

### Binary `/` (`012`)

```
	mov	ax,word ptr [bp-2]
	cwd	
	idiv	word ptr [bp-4]
```

`cwd` (no operands) sign-extends AX into DX:AX before the signed
divide. After `idiv` the quotient is in AX and the remainder is in DX.
For `/` we use AX as-is.

`cwd` has a trailing TAB followed by CRLF (`\tcwd\t\r\n`) — same shape
as `ret\t\r\n`: every operand-less mnemonic gets the empty-operand TAB.

### Binary `%` (`013`)

```
	mov	ax,word ptr [bp-2]
	cwd	
	idiv	word ptr [bp-4]
	mov	ax,dx
```

Same machinery as `/`, with `mov ax,dx` afterwards to surface the
remainder.

### Binary `&` / `|` / `^` (`014`/`015`/`016`)

Two-operand, same shape as `+`/`-`:

```
	mov	ax,word ptr [bp-2]
	and	ax,word ptr [bp-4]
```

`or` and `xor` use the same pattern. (BCC's `xor ax,ax` "set zero"
pattern is just this same instruction encoding hit on a special case.)

### Shifts `<<` / `>>` (`017`/`018`)

```
	mov	ax,word ptr [bp-2]
	mov	cl,byte ptr [bp-4]
	shl	ax,cl
```

- Shift count goes through `CL` (only the low 8 bits of the right
  operand are used).
- The right operand is loaded as `byte ptr [bp-N]` even when the
  source variable is an `int` — BCC reads only its low byte.
- For `<<` BCC emits `shl ax,cl`; for `>>` on a signed `int` it emits
  **`sar ax,cl`** (signed arithmetic right shift, preserving sign bit).
- 8086 has no `shl reg,imm` (added in 80186) so this is the only
  encoding available pre-80186. BCC stays on the 8086-compatible
  encoding even when the right operand is a compile-time constant —
  we'll need a constant-rhs shift fixture to confirm this.

### Comparison-as-value (`019`–`024`)

`return x < y;` produces this six-instruction pattern, with the
specific conditional-jump mnemonic varying per operator:

```
	mov	ax,word ptr [bp-2]
	cmp	ax,word ptr [bp-4]
	j<inv>	short @<F>        ; jump if comparison is FALSE
	mov	ax,1              ; true: AX = 1
	jmp	short @<E>        ; skip the false case
@<F>:                             ; false-branch
	xor	ax,ax             ; AX = 0
@<E>:                             ; end
```

The `j<inv>` is the *inverse* of the source operator:

| C operator | jump-if-false |
| ---------- | -------------- |
| `==`       | `jne`          |
| `!=`       | `je`           |
| `<`        | `jge`          |
| `>`        | `jle`          |
| `<=`       | `jg`           |
| `>=`       | `jl`           |

All are signed-comparison mnemonics (`jl/jg/jle/jge`), not the unsigned
variants (`jb/ja/jbe/jae`), because BCC treats `int` as signed.

### Comparison in a condition with an immediate RHS (`025`)

When the condition is `<var> == <immediate>`, BCC skips the load-to-AX
and emits a memory-immediate compare directly:

```
	cmp	word ptr [bp-2],0
	jne	short @<skip>
```

vs. the local-local form which still goes through AX:

```
	mov	ax,word ptr [bp-2]
	cmp	ax,word ptr [bp-4]
	jge	short @<else>
```

The local-local form for an if-condition uses the *same inverse-jump
table* as comparison-as-value above; the difference is only that no 0/1
materialization is needed when the result feeds a conditional jump.

### `if` without `else` (`025`)

```
	<cond>
	j<inv> short @<skip>
	<then-stmts>
@<skip>:
	<following-stmts>
```

The `<skip>` label is the fall-through point; any code after the `if`
runs there.

### `if`/`else` (`026`)

```
	<cond>
	j<inv> short @<else>
	<then-stmts>
	jmp short @<end>          ; <— always emitted, even when unreachable
@<else>:
	<else-stmts>
@<end>:
	<following-stmts>
```

BCC emits the unconditional `jmp short @<end>` between the then-branch
and the `<else>:` label **even when the then-branch ends with a
return** (making it unreachable). For byte-exact output we have to emit
it too — no dead-code elimination in this path.

When the if-else has nothing following it (as in fixture 026, where
both branches return), the `<end>` label coincides with the function
exit.

## Label numbering

Every label takes the form `@<func-idx>@<N>` where N is computed as:

```
N = 50 + 24 * slot
```

`slot` is an index that increments per "label slot" reserved by the
function. Each control construct reserves a fixed number of slots; the
function exit gets the next slot after the whole body has been planned.

Per-construct reservations and which slots actually emit a label:

| Construct                      | Slots | Emits at offset(s)   | Notes                                          |
| ------------------------------ | :---: | -------------------- | ---------------------------------------------- |
| Comparison-as-value            | 3     | +1 (false), +2 (end) | offset +0 unused                              |
| `if` (no else)                 | 2     | +1 (skip)            | +0 unused                                      |
| `if`/`else`                    | 3     | +2 (else)            | +0 and +1 unused; `end` re-uses the exit slot  |
| `while`                        | 3     | +0 (body), +1 (check) | +2 unused                                    |
| (function exit, always last)   | 1     | the single slot      | every function has this                       |

Verified against fixtures:

- `001`: 1 slot → exit at slot 0 → `@1@50`. ✓
- `019`: cmp-as-value 3 + exit 1 = 4 → exit at slot 3 → `@1@122`. ✓
- `025`: if-no-else 2 + exit 1 = 3 → exit at slot 2 → `@1@98`. ✓
- `026`: if-else 3 + exit 1 = 4 → exit at slot 3 → `@1@122`. ✓
- `027`: while 3 + exit 1 = 4 → exit at slot 3 → `@1@122`. ✓

The "unused" slots are presumably reservations BCC makes before
knowing which branches will actually need a target.

## Register allocation for locals

Even at default `-S -ms` (no `-O`), BCC enregisters some local `int`
variables into `SI`, `DI`, `DX`, and `BX` in that order. Investigation
fixtures `028`–`032` pin the heuristic and the emission shape.

### When does a local get a register?

| Fixture | Local  | Uses (incl. init) | Lives in |
| ------- | ------ | :---------------: | -------- |
| `027`   | `i`    | 4                 | SI       |
| `028`   | `i`    | 4                 | SI       |
| `029`   | `i`    | 4                 | SI       |
| `029`   | `sum`  | 4                 | DI       |
| `030`   | `limit`| **2**             | **stack** (`[bp-2]`) |
| `030`   | `i`    | 5                 | SI       |
| `031`   | `a`    | 5                 | SI       |
| `031`   | `b`    | 4                 | DI       |
| `031`   | `c`    | 4                 | DX       |
| `031`   | `d`    | 4                 | BX       |
| `032`   | `i`    | 3                 | SI       |
| `046`   | `a`–`d`| 3 each            | DI/DX/BX/CX (source order) |
| `046`   | `x`    | 4 (most-used)     | SI       |
| `048`   | `a`    | 5 (most-used)     | SI       |
| `048`   | `b`–`x`| 4 each            | DI/DX/BX/CX (source order) |
| `048`   | `y`    | 4                 | stack (`[bp-2]`, 6th eligible) |

The heuristic, refined across fixtures 028–048:

1. A local or parameter with ≥ 3 textual occurrences (counting the
   "implicit init" of a declaration or a param entering the function)
   is eligible for a register.
2. The eligible **most-used** name gets `SI`. Ties are broken by
   source order — earliest wins.
3. The remaining eligibles fill `[DI, DX, BX, CX]` in source order.
4. Beyond five eligibles, the rest spill to the stack (fixture 048
   confirms the pool size: AX is BCC's working register, SP/BP belong
   to the frame, so the maximum is `{SI, DI, DX, BX, CX}` = 5).

Only `SI` and `DI` are pushed/popped as callee-saved. `DX`, `BX`, and
`CX` are used without save/restore — fine for leaf functions, and
inherited as-is for non-leaf functions today (no fixture pins the
behavior when a register-promoted variable's lifetime overlaps a call
that clobbers DX/BX/CX).

### Prologue and epilogue shape

Stack space is allocated **before** the callee-saved pushes. Only SI
and DI are saved/restored (DX and BX are used without saving, fine as
long as nothing the function calls clobbers them):

```
	push	bp
	mov	bp,sp
	dec	sp / sub sp,N        ; only if there are stack-resident locals
	push	si                   ; only if SI is used
	push	di                   ; only if DI is used
	...body...
	pop	di                   ; reverse of the pushes
	pop	si
	mov	sp,bp                ; only if there were stack-resident locals
	pop	bp
	ret
```

In fixture `030` with `limit` on the stack and `i` in SI, the prologue
is `push bp / mov bp,sp / dec sp / dec sp / push si`, and the epilogue
is `pop si / mov sp,bp / pop bp / ret`. With *only* register locals
(028, 029, 031, 032), there is no `dec sp` / `sub sp,N` and no
`mov sp,bp` in the epilogue.

### Initializing a register local

Same constant-folding rules as for AX:

- `int i = 0;` → `xor si,si` (and same for any register: `xor di,di`,
  `xor dx,dx`, `xor bx,bx`).
- `int i = K;` (K ≠ 0) → `mov <reg>,K`.
- Non-constant initializer (not yet observed): presumably load to AX
  via the usual path, then `mov <reg>,ax`.

### Assignment to a register local

```
	i = i + 1;
```

emits (when `i` is in SI):

```
	mov	ax,si
	inc	ax
	mov	si,ax
```

Two things to note:

1. **`x ± 1` on a value already in AX becomes `inc ax` / `dec ax`**,
   not `add ax,1` / `sub ax,1`. **`x ± 2`** also folds — `r + 2`
   compiles to *two* `inc ax`s in a row (2 bytes total vs. 3 for
   `add ax,2`). Fixture 076 case 1 pins the `±2` half of this. At
   `±3` and beyond, the cost of three or more inc/dec ties with
   `add/sub ax, K` (3 bytes) and BCC switches back to the `add`/`sub`
   form (fixture 076 case 2: `r + 3` → `add ax, 3`).
2. The store back is via AX even though the rhs is computed in AX —
   BCC doesn't fuse the operation into the destination register. The
   shape is always `mov ax,<reg>` / `<op>` / `mov <reg>,ax`.

### Reading a register local in an expression

The plain load `mov ax, word ptr [bp-N]` becomes `mov ax, <reg>`:

```
	return a + b + c + d;       ; all four in SI/DI/DX/BX
```

```
	mov	ax,si       ; a
	add	ax,di       ; b
	add	ax,dx       ; c
	add	ax,bx       ; d
```

So `add ax, <reg>` is used directly instead of `add ax, word ptr [bp-N]`.

### Comparison with a register operand

When the LHS of a comparison-in-condition is a register local, BCC
skips the load-to-AX and compares directly:

```
	cmp	si,10                ; cmp <reg>, K (fixture 027/032)
	cmp	si,word ptr [bp-2]   ; cmp <reg>, [stack-local]  (fixture 030)
	cmp	si,di                ; cmp <reg>, <reg>          (hypothetical;
	                             ; not yet captured)
```

The conditional-jump mnemonic obeys the same true/inverse selection as
the load-via-AX form.

### `for` and `do-while` (`061`, `062`, `065`)

`for (init; cond; step) body` follows the while pattern but with a
per-iteration step inlined between the body and the check:

```text
   <init>
   jmp short @check
@body:
   <body>
   <step>
@check:
   <cond evaluation, j-true @body>
@break-target:                       ; emitted only if `break;` in body
```

The init runs once before the loop. The step is *inlined at the end
of the body* — no separate label unless `continue;` appears.

`do body while (cond);` is simpler — no trampoline jmp:

```text
@body:
   <body>
   <cond evaluation, j-true @body>
```

#### Slot reservation for `for`

`for` reserves the body slot first, then walks the body, then reserves
a continue-target slot **only when the body itself reserved no slots**,
then check + break-target. Fixture 061 (body = a plain assignment, 0
nested labels) reserves 4 slots total; fixture 065 (body = if-no-else,
2 nested labels) reserves 5. The "filler" slot in the body-empty case
is the would-be continue-target — unused as a label when `continue;`
isn't present.

### `break` and `continue` (`063`, `064`, `065`, `066`)

`break;` emits a `jmp short` to the loop's `@break-target` label.
`continue;` emits a `jmp short` to the loop's continue-target — which
for `while` / `do-while` is the same as the `@check` label.

The `@break-target` label is **only emitted when the loop body
actually contains a `break;`** (fixture 063 emits it; 027 does not,
even though the slot is reserved).

Nested loops: `break;` / `continue;` target the **innermost**
enclosing loop. Fixture 066 (two nested `while`s, `break;` in the
inner) shows the inner loop's `@break-target` reached only by the
inner break — the outer loop continues iterating.

### Use-count rule refinement: uninitialized declarations

`int x;` (declaration without initializer) does **not** contribute
to `x`'s use count for the SI-priority tie-break. Only initializing
declarations count as a use. Fixture 066: `int i = 0; int j;`
declares both, but BCC places `i` (initialized + 4 textual uses = 5)
in SI rather than `j` (uninitialized + 5 textual uses = 5) — the tie
is broken in `i`'s favor because the `int j;` declaration doesn't add
to `j`'s count.

### `while` loop codegen

```c
while (<cond>) { <body> }
```

becomes (with slot base reserved by the planner):

```
	jmp	short @<check>          ; jump to the condition first
@<body>:                                ; slot +0
	<body-stmts>
@<check>:                               ; slot +1
	<cond>
	j<true>	short @<body>          ; true-mnemonic, NOT inverse
```

Two contrasts with the if/if-else pattern:

- **Trampoline `jmp` to the check before the body.** The condition is
  evaluated at the bottom of the loop.
- **The conditional jump uses the *true*-mnemonic** (`jl` for `<`,
  `je` for `==`, …), because we jump *back to the body when the
  condition holds*. (Inverse-mnemonic jumps fall through into the
  successor for `if`.)

Slot layout: while reserves 3 slots: `+0 body`, `+1 check`, `+2 unused`.
(The `+2` is presumably the reservation for a future `break` / `continue`
target; BCC seems to over-reserve consistently.)

## Char register allocation (`047`, `050`–`055`)

`char` locals and parameters participate in their own register pool,
separate from the int pool but with allocation rules that interact
with the function's call shape.

### Char register pool: `[DL, BL, CL]`

Chars draw from `{DL, BL, CL}` in **source order**. Fixture 050
(`char a, b, c`, all enregistered) lays them down in exactly that
sequence. AL is the working byte (used for arithmetic and the
load/cbw round-trip); AH/BH/CH/DH are unused by BCC for variables.

### Char enregistration is suppressed when the function makes a call

Fixture 055 (`int main(void) { char c = 5; ++c; return f(c); }`) shows
`c` on the stack at `[bp-1]` even though it has 4 textual uses. The
reason: DL/BL/CL all alias with the *caller-clobbered* halves of
DX/BX/CX, and BCC's call protocol does not save them. A char that
must survive a call has to live on the stack.

Today we suppress char enregistration for the whole function whenever
its body contains *any* `Call` expression. (Ints aren't similarly
restricted — none of our fixtures exercise an int that lives across a
call, so we leave that path alone until a fixture forces a choice.)

### Char codegen in a register

| Form              | Asm (target in DL)                                |
| ----------------- | ------------------------------------------------- |
| `char c = K;`     | `mov dl,K`                                        |
| `++c;` / `--c;`   | `mov al,dl` / `inc al` (or `dec`) / `mov dl,al`   |
| `return c;`       | `mov al,dl` / `cbw` (sign-extend AL into AX)      |
| `c < K` (cond)    | `cmp dl,K`                                        |

Notable: BCC does **not** emit `inc dl` directly — even though `INC r8`
is a valid 8086 instruction, BCC always routes through AL. And the
zero-init special case (`xor r,r` for 16-bit) doesn't apply to byte
registers; `char c = 0;` is `mov dl,0`, not `xor dl,dl`.

### Char on the stack

When a char isn't enregistered (or never qualified), it sits at a
`byte ptr [bp-N]` slot with the standard alignment rules. `++` /
`--` and reads use the same AL round-trip as the register form:

```
	mov	al,byte ptr [bp-1]
	inc	al
	mov	byte ptr [bp-1],al
```

### Char parameters

Char params live in 2-byte slots on the stack (the caller pushes a
full word; only the low byte is meaningful). The callee reads them
as `byte ptr [bp+N]`:

```
_f	proc	near
	push	bp
	mov	bp,sp
	mov	dl,byte ptr [bp+4]     ; char c register-promoted
```

If a char param isn't enregistered, no copy happens — reads go
directly to `[bp+N]`.

### Char arguments at the call site

Caller-side, char args are loaded into AL (8-bit) before the
standard `push ax`:

```
	mov	al,1                   ; constant char arg
	push	ax
	; or:
	mov	al,byte ptr [bp-1]     ; char-on-stack arg
	push	ax
```

BCC consults the callee's declared parameter types to pick the byte
form, so our codegen needs a translation-unit-wide signature table
(see `codegen::Signatures`). Calls to functions with no in-TU
definition fall back to the int form — no fixture pins extern char
arguments yet.

### Frame alignment with chars

Fixture 055 forces a single-char stack frame: `dec sp` only once
would leave SP at an odd offset. BCC instead emits **two** `dec sp`s
— the frame is rounded up to an even byte count. We pad the local
allocation to a 2-byte boundary at the end of layout.

## Local variable alignment

Fixture 011 captures `char c; int i;` — total 3 bytes of values, but
BCC allocates **4 bytes**:

```
	sub	sp,4
	mov	byte ptr [bp-1],1     ; char c at [bp-1]
	mov	word ptr [bp-4],2     ; int  i at [bp-4]
```

The byte at `[bp-2]` is padding so the `int` lands on an even-offset
slot. So:

- `char` occupies 1 byte at `[bp-N]`, no padding *before* it.
- `int` requires a 2-byte-aligned bp-offset; when the cursor sits on
  an odd offset (because a `char` preceded it), BCC inserts a 1-byte
  pad and the int lands at the next even offset.
- The frame size is exactly the cumulative used offset; no extra tail
  padding has been observed.

This sidesteps the "is a 3-byte frame possible?" question: in normal
C source it isn't, given BCC's alignment policy. The `dec sp` ↔
`sub sp,N` threshold (≤2 → `dec sp`, >2 → `sub sp,N`) appears safe.

Open: a `char`-only frame (e.g. `char c;` alone) — does BCC emit
`dec sp` (1-byte frame) or round up to 2? Needs a fixture.

`char` initialization with a constant uses `mov byte ptr [bp-N], K`
(byte-immediate). Non-constant char initialization, char reads in
expressions, and char enregistration are all unexercised — needs
fixtures before we can pin the codegen.

### Calling a function (`010`, `033`–`035`)

```
	call	near ptr _f
```

- Small-memory-model: all calls are **near**, but BCC writes
  `near ptr` explicitly (TASM accepts both with and without; the explicit
  form is the bytes BCC produces).
- Calling convention is cdecl: caller pushes args **right-to-left**,
  result lives in AX, caller cleans the stack.
- Arguments are always materialized into AX first, then pushed:
  `mov ax, K / push ax`. BCC does *not* use 80186+ `push K`.
- Cleanup after the call: BCC uses `pop cx` per arg when there are
  ≤ 2 args (small/byte-counted encoding), and switches to
  `add sp, N*2` at ≥ 3 args (one 3-byte instruction is smaller than
  3+ `pop cx`s). Confirmed by 010 (0 args, nothing), 033 (1), 034 (2),
  049 (3 → `add sp,6`), 046/048 (4 → `add sp,8`).

```
	mov	ax,5            ; rightmost arg first
	push	ax
	mov	ax,3            ; then the next
	push	ax
	call	near ptr _f
	pop	cx              ; one per arg
	pop	cx
```

For `return f(args);`, the result is in AX after the call, then the
standard `jmp short @<f>@50` to the exit. No move needed.

### Parameter access in the callee (`033`–`035`)

After the standard small-model prologue (`push bp / mov bp,sp`), the
stack layout *above* `bp` is:

| Offset    | Contents                                    |
| --------- | ------------------------------------------- |
| `[bp+0]`  | saved `bp`                                  |
| `[bp+2]`  | near-call return address (2 bytes)          |
| `[bp+4]`  | first argument (pushed last by caller)      |
| `[bp+6]`  | second argument                             |
| `[bp+N]`  | further arguments at +2 each                |

So the **leftmost** parameter sits closest to `bp`. Each `int`/`char`/
pointer arg takes a 2-byte slot — `char` arguments are promoted at the
caller's push site to a 2-byte push (the high byte is undefined). A
`long` arg takes **4 bytes** — its low half sits at the lower bp-offset
and its high half at the next 2-byte offset. Fixture 285 (`long f(long
a, long b)`) reads `a` from `[bp+4]/[bp+6]` and `b` from `[bp+8]/[bp+10]`.
This means the second-parameter offset jumps depending on the first
parameter's width; our `locals` layout pass walks param types one by
one and adds 4 for `long`/`unsigned long`, 2 for everything else.

In a medium or large memory model the return address grows to 4 bytes,
shifting the first arg to `[bp+6]`. We currently only handle `-ms`.

#### Register-promoted parameters (`035`)

Parameters participate in the same register-allocation heuristic as
locals: an `int` param with ≥ 3 textual occurrences (counting the
"implicit init" of the param entering the function) gets a register
from the `[SI, DI, DX, BX]` pool, in **source order, params before
locals**.

The prologue gains a per-promoted-param load **after** the callee-save
pushes:

```
	push	bp
	mov	bp,sp
	push	si           ; callee-save (because we'll clobber it)
	push	di           ; callee-save
	mov	si,word ptr [bp+4]   ; load incoming arg `x` into SI
	; ... local inits begin here ...
```

Stack-resident params stay at their incoming `[bp+N]` slot — no
spill/copy is performed.

### `cmp <reg>, 0` peephole: `or <reg>, <reg>` (`035`)

When the LHS of a comparison-with-zero is a register, BCC substitutes
the smaller `or <reg>, <reg>`:

```
	or	si,si             ; instead of `cmp si,0`
	jg	short @1@50
```

The `or` sets ZF/SF/PF identically to `cmp <reg>, 0` (it computes
`reg | reg == reg`, sets flags based on the result) and clears OF/CF
— matching what `cmp` against an immediate would produce, so the
signed conditional jumps (`jg/jl/jge/jle/je/jne`) all give the right
answer. The encoding is 2 bytes vs `cmp <reg>, 0` at 3+ bytes.

We don't yet have a fixture for `cmp <stack>, 0` — that path may
still use the memory-immediate form, since `or` would write back to
memory.

### Compound assignment (`067`–`071`)

`x op= y` is **not** equivalent to `x = x op y` at the asm level —
BCC routes it through a much tighter codegen path. When the target
is a register-resident `int`, the operation hits the register
directly without going through AX.

| Form                | Asm (target in `<reg>`)              |
| ------------------- | ------------------------------------ |
| `x += 1;`           | `inc <reg>`                          |
| `x -= 1;`           | `dec <reg>`                          |
| `x += K;` (K ≠ 1)   | `add <reg>, K`                       |
| `x -= K;` (K ≠ 1)   | `sub <reg>, K`                       |
| `x &= K;`           | `and <reg>, K`                       |
| `x \|= K;`          | `or <reg>, K`                        |
| `x ^= K;`           | `xor <reg>, K`                       |
| `x += y;` (y in mem)| `add <reg>, word ptr [bp-N]`         |
| `x *= K;`           | `mov dx, K / mov ax, <reg> / imul dx / mov <reg>, ax` |

The `*=` form is the exception: `imul reg, imm` is 80186+ only, so
BCC stays on the 8086-compatible single-operand `imul` and routes
through AX. The multiplier goes into DX *first*, then AX is loaded
from the register — note the order matters.

This asm-level distinction is significant for fingerprinting / 
decompilation: a function that contains `add <reg>, K` directly was
almost certainly compiled from `x += K`, while one that emits
`mov ax, <reg> / add ax, K / mov <reg>, ax` would have been
`x = x + K`. The two source forms are equivalent in semantics but
distinguishable in compiled output.

### `++` / `--` (`040`–`045`)

`++x` and `--x` count as **two textual uses** of `x` in the
register-allocation heuristic (read + write), matching what
`x = x + 1` would contribute.

When the target is a register-resident local/param, BCC emits a single
instruction — the `mov ax / inc ax / mov` round-trip used for
`x = x + 1` is bypassed:

| Form         | Asm (target in a register)                |
| ------------ | ----------------------------------------- |
| `++x;`       | `inc <reg>`                               |
| `--x;`       | `dec <reg>`                               |
| `x++;`       | `inc <reg>` (value discarded — same as pre) |
| `x--;`       | `dec <reg>`                               |
| `return ++x;` | `inc <reg>` / `mov ax, <reg>`           |
| `return x++;` | `mov ax, <reg>` / `inc <reg>`           |

Statement and expression forms differ only when the value is *used*:
the expression form must materialize the new value (pre) or the old
value (post) in AX. The statement form omits the `mov ax, <reg>`.

Open: `++/--` on a stack-resident target. The natural extension is
`inc word ptr [bp-N]` (the 8086 supports memory-operand INC/DEC), but
in every fixture so far BCC has chosen to enregister the target — we
can't yet confirm that without forcing a stack-allocated target,
which depends on the deferred 5-register-pool / char-register-allocation
work (fixtures 046, 047).

### Logical `&&` and `||` (`056`–`060`)

Short-circuit evaluation. Each operand is tested with the standard
"test-and-jump" shape, except neither operand materializes a 0/1
value — they branch directly. Comparison operands use their natural
inverse/true-mnemonic jump; non-comparison operands are tested
against zero (`cmp <stack>, 0` or `or <reg>, <reg>` for register
operands; both set ZF the same way).

#### `&&` and `||` in condition position (056, 057, 058)

The if's skip label serves as the "false target" for both operators.
`||` additionally needs a "then-entry" label so an early-true operand
has somewhere to land — the if's `base+0` slot (otherwise unused for
a plain cond) is repurposed for this.

`if (x && y) <then>`:
```text
   cmp word ptr [bp-2], 0
   je  short @skip
   cmp word ptr [bp-4], 0
   je  short @skip
   <then>
@skip:
```

`if (x || y) <then>`:
```text
   cmp word ptr [bp-2], 0
   jne short @then-entry
   cmp word ptr [bp-4], 0
   je  short @skip
@then-entry:
   <then>
@skip:
```

When operands are themselves comparisons (058), each emits its
natural cmp + inverse jump targeting the skip (for `&&`) or the
then-entry / skip pair (for `||`).

#### `&&` and `||` in expression position (059, 060)

When the result is consumed as a value (e.g., `return a && b;`),
BCC materializes a 0 or 1 in AX after the short-circuit evaluation.
Both operators reserve **4 slots**: +0 unused, +1 unused (`&&`) or
true-mat (`||`), +2 false-mat, +3 end.

`return x && y`:
```text
   cmp word ptr [bp-2], 0
   je  short @false-mat
   cmp word ptr [bp-4], 0
   je  short @false-mat
   mov ax, 1
   jmp short @end
@false-mat:
   xor ax, ax
@end:
```

`return x || y`:
```text
   cmp word ptr [bp-2], 0
   jne short @true-mat
   cmp word ptr [bp-4], 0
   je  short @false-mat
@true-mat:
   mov ax, 1
   jmp short @end
@false-mat:
   xor ax, ax
@end:
```

The `||` form's true-mat label consolidates two paths: short-circuit
from an early-true operand, and fall-through when the last operand
was true (i.e., the `je @false-mat` didn't fire).

Open: chained or nested `&&`/`||` (e.g., `a && b && c`, `(a || b) && c`)
— each non-final operand's short-circuit-to-true still needs a jump,
not fall-through, so the simple binary recursion we use today doesn't
generalize without an extra "is-final-leaf" hint. Logical operators
in `while` conditions are also unobserved.

### Unary operators (`036`–`039`)

| C source            | Asm                                              |
| ------------------- | ------------------------------------------------ |
| `return -5;`        | `mov ax,65531` (constant-folded, u16-wrapped)    |
| `return -x;`        | `mov ax,[bp-N]` / `neg ax`                       |
| `return ~x;`        | `mov ax,[bp-N]` / `not ax`                       |
| `return !x;`        | `mov ax,[bp-N]` / `neg ax` / `sbb ax,ax` / `inc ax` |

Negative integer constants are emitted as their **unsigned-wrapped
16-bit** decimal representation: `-5` → `mov ax,65531`. So immediate
emission narrows the (internally 32-bit) folded value to 16 bits
before formatting.

The `!x` shape is the classic 8086 zero-test idiom:

- `neg ax` — sets CF = (ax != 0); ax becomes `-x`.
- `sbb ax,ax` — `ax := ax - ax - CF = -CF`, so ax is `0` if x was 0,
  `0xFFFF` otherwise.
- `inc ax` — `0 → 1`, `0xFFFF → 0`.

No conditional jumps, no labels — four straight-line instructions.
This makes `!x` significantly smaller than a comparison-as-value
expansion of `x == 0`.

### `switch` statement (`072`, `075`; jump-table `073`, `076`; linear `074`)

BCC picks one of three dispatch strategies for a switch statement
based on the case-value pattern:

- **Chained compares** — for a small number of cases or any case
  set that's neither dense-from-zero nor sparse-with-many-arms.
  Fixtures 072 (3 cases) and 075 (2 cases + default).
- **Jump table** — for ≥ 4 contiguous cases starting at 0. Fixtures
  073 (8 cases 0..7) and 076 (4 cases 0..3).
- **Linear value search** — for sparse case sets with ≥ 4 cases.
  Fixture 074 (cases 1, 10, 100, 1000).

#### Chained-compare dispatch (`072`, `075`)

Load scrutinee into AX once, then one `cmp + je` per case
(in source order), then a trailing `jmp` to either the
default body (if present) or the end-of-switch label:

```
	mov	ax,word ptr [bp-2]   ; load scrutinee
	or	ax,ax                 ; case 0 uses the `or` peephole
	je	short @1@170          ; ↳ case 0 body
	cmp	ax,1
	je	short @1@194          ; ↳ case 1 body
	cmp	ax,2
	je	short @1@218          ; ↳ case 2 body
	jmp	short @1@242          ; ↳ end-of-switch (no default)
```

`case 0` triggers the `or ax,ax` short form (same one the `cmp
<reg>,0` peephole uses elsewhere). The trailing `jmp` targets the
default body if the switch has one; otherwise it falls past the
last case to the end-of-switch label.

Case bodies are emitted in source order — including default. Each
arm gets its own label; bodies that include `break;` jump to the
end-of-switch label. Bodies without `break;` fall through into the
next arm's body (chained-fallthrough is implied but unobserved —
fixture 076 falls through but uses the jump-table strategy).

#### Slot reservation

Each switch reserves a fixed number of *pre-dispatch* slots that
get burned by the slot counter even though no labels actually
land on them. The count depends on the dispatch strategy:

| Strategy        | Pre-slots                      |
|-----------------|--------------------------------|
| Chained         | `#non-default-cases + 2`       |
| Jump-table      | `3` (fixed)                    |
| Linear-search   | `#cases + 2`                   |

Then one slot per case body (in source order, default included),
then one slot for the end-of-switch label that `break;` targets.
For 072 (3 cases, no default): `5 + 3 + 1 = 9` slots → end at
slot 8 (label `@1@242`), function exit at slot 9 (`@1@266`).
For 075 (2 cases + default): `4 + 3 + 1 = 8` slots → end at
slot 7 (`@1@218`), exit at slot 8 (`@1@242`).

#### Jump-table dispatch (`073`, `076`)

```
	mov	bx,word ptr [bp-2]
	cmp	bx,3                 ; bounds check (max case value)
	ja	short @1@218         ; out-of-range → end-of-switch
	shl	bx,1                 ; index = value * 2 (word entries)
	jmp	word ptr cs:@1@C876[bx]
```

Two important shape details:

- BCC uses **BX** (not AX) for the scrutinee — the only register
  encoding for `jmp word ptr cs:LBL[reg]` indexed addressing.
- The bounds check uses `ja` (unsigned), so negative scrutinees
  also fail it (their two's-complement wrap puts them above the
  max).

The address table is emitted as data *after* `_main endp` but
before `?debug C E9`:

```
_main	endp
@1@C876	label	word
	dw	@1@122
	dw	@1@146
	dw	@1@170
	dw	@1@194
```

The data label uses the `@<func>@C<num>` form. The `C` prefix
distinguishes data labels from code labels; **the `<num>` value
does not follow the `50 + 24·k` code-label scheme** and we don't
yet know what determines it. Empirically, fixtures 073 (8 cases →
`C1244`) and 076 (4 cases → `C876`) both fit `92·n + 508`, but
the constants `92` and `508` have no obvious source — this could
be a coincidence of two data points. _See "Open questions"._

#### Linear-search dispatch (`074`)

For sparse cases (≥ 4 with non-contiguous values), BCC spills the
scrutinee, loads CX with the case count and BX with a pointer to
a parallel value table, then loops:

```
	mov	ax,word ptr [bp-2]
	mov	word ptr [bp-4],ax    ; spill scrutinee (extra stack slot)
	mov	cx,4                   ; case count
	mov	bx,offset @1@C738
@1@98:
	mov	ax,word ptr cs:[bx]
	cmp	ax,word ptr [bp-4]
	je	short @1@170           ; ↳ dispatch jmp
	inc	bx
	inc	bx
	loop	short @1@98
	jmp	short @1@290           ; not found → end-of-switch
@1@170:
	jmp	word ptr cs:[bx+8]     ; +8 = offset to address table
```

The dispatch indirect-jmp uses `[bx + 2·case_count]` to land on
the address table entry corresponding to the matched value (since
the values table comes first and is `2·N` bytes long).

The spill slot adds a 2-byte chunk to the stack frame — BCC
allocates it AFTER user locals, so for fixture 074 (`int x` at
`[bp-2]`) the spill lands at `[bp-4]` and the prologue uses `sub
sp,4` (vs. `dec sp` ×2 for fixtures without spill).

Two parallel tables emitted after `_main endp`: values then
addresses. **Values are written byte-by-byte (`db`)** as
little-endian halves of the 16-bit value (`1000` → `db 232 / db
3`), which is a notable byte-exact fingerprint quirk — most other
compilers emit `dw 1000`.

```
_main	endp
@1@C738	label	word
	db	1
	db	0
	db	10
	db	0
	db	100
	db	0
	db	232
	db	3
	dw	@1@194
	dw	@1@218
	dw	@1@242
	dw	@1@266
```

Fixture 074 (4 cases → `C738`) fits the empirical formula
`74·n + 442`, but with only one linear-search data point this is
not yet a confirmed rule.

### Constant folding (`005`)

BCC folds simple arithmetic on integer literals at compile time. Source
`return 1 + 2;` emits exactly:

```
	mov	ax,3
	jmp	short @1@50
```

No `add` is generated. This means our front-end has to actually
recognize `1 + 2` as a binary expression (we can't skip parsing it),
then a fold pass replaces it with the constant `3` before codegen.

## Arrays and pointers (`077`–`082`)

### Array layout on the stack

`<elem-type> a[N]` allocates `N * sizeof(<elem-type>)` bytes on the
stack and places element 0 at the **most negative** bp-offset (the
"bottom" of the array's stack chunk). Later indices step toward `bp`.
For `int a[3]` (fixture 077), `a[0]` is at `[bp-6]`, `a[1]` at
`[bp-4]`, `a[2]` at `[bp-2]`. For `char a[4]` (fixture 082),
`a[0]` is at `[bp-4]`, `a[1]` at `[bp-3]`, etc.

Arrays never enregister — their name is implicitly an address, and
register-resident locals have no address. The locals analyzer skips
them when handing out registers.

### Constant-indexed array access (`077`, `078`, `082`)

When the index folds to a constant, the access lowers to a plain
stack reference with no address computation:

```
mov ax, word ptr [bp-6]      ; int a[0]
mov word ptr [bp-6], 5       ; int a[0] = 5
mov al, byte ptr [bp-4]      ; char a[0] (then `cbw` to widen to int)
mov byte ptr [bp-4], 88      ; char a[0] = 88
```

Identical shape to a non-array local at the same offset, but reading
the asm doesn't betray the difference — the C-level distinction is
lost at the asm level.

### Variable-indexed array access (`079`)

When the index is a variable, BCC uses the same 5-instruction
effective-address sequence for both reads and writes:

```
mov  bx, <index>             ; copy index to bx
shl  bx, 1                    ; stride 2 for int (omitted for char, stride 1)
lea  ax, word ptr [bp-N]      ; ax = &a[0]
add  bx, ax                   ; bx = &a[i]
mov  word ptr [bx], <rhs>     ; or mov ax, word ptr [bx] for a read
```

Note BCC writes `lea ax, word ptr [bp-N]` with the `word ptr`
annotation preserved — `lea` doesn't actually load memory, so the
prefix is meaningless to the CPU but BCC emits it consistently.
The address goes via AX (`lea ax / add bx, ax`) rather than `lea
bx, ...` directly — same AX-as-working-register pattern that
shows up for `&x`.

For char arrays (stride 1), the `shl bx, 1` is omitted. No fixture
yet pins this — derived from the structure.

### Address-of (`080`)

`&<name>` lowers to:

```
lea  ax, word ptr [bp-N]
mov  <dst>, ax
```

The address always materializes in AX first, then transfers to
the destination — BCC doesn't use `lea <dst>, ...` directly even
when it could. Consistent with the "AX is the working register"
pattern (e.g. compound-assignment-via-AX for `*= K`).

Taking the address of `x` forces `x` to be stack-resident, even
if its use count would otherwise qualify it for a register.

### Pointer storage and enregistration

Pointers occupy 2 bytes (16-bit near pointers under the small
memory model). They share the int register pool (`SI, DI, DX, BX,
CX`) but enregister at a **lower use threshold** — `≥ 2` instead of
`≥ 3` for ints. Both fixtures 080 and 081 have pointers with
exactly 2 uses (init + one deref) and both put the pointer in SI.

The likely reason: pointer use almost always involves indirect
addressing (`mov ax, [reg]`), which has no equivalent stack-source
form, so keeping a pointer on the stack costs an extra load per
access. The threshold drop preempts that overhead.

### Pointer dereference (`080`, `081`)

`*<ptr>` in rvalue position: `mov ax, word ptr [<reg>]` where
`<reg>` holds the pointer (SI in our fixtures).

`*<ptr> = <value>;` (lvalue position, constant rhs): `mov word ptr
[<reg>], <value>` — a direct indirect store of the immediate.

The deref width is `word ptr` for `int *`, `byte ptr` for `char *`
(no fixture for the latter yet — inferred from the symmetry).

## Structs and typedef (`101`–`106`)

### Layout: packed fields, even total size

BCC packs struct fields tightly with **no inter-field padding**:
`{char c; int n;}` lands `c` at offset 0 and `n` at offset 1 — the
int is misaligned. The **total size** rounds up to an even
number of bytes (a 3-byte struct gets 1 byte of trailing pad to
become 4). Fixture 102 demonstrates: `sub sp, 4` with `c` at
`[bp-4]` and `n` at `[bp-3]`.

| Struct                  | Field offsets | Total size |
|-------------------------|---------------|------------|
| `{int x;}`              | 0             | 2          |
| `{char c; int n;}`      | 0, 1          | 4 (3 + pad)|
| `{int x; int y;}`       | 0, 2          | 4          |

The struct itself has alignment 2 (so a struct followed by an int
local stays word-aligned), but the *contents* don't pad to fit
that alignment internally.

### `.` access on a struct local (`101`–`103`)

`a.x` lowers to a plain stack-local reference at the bp-offset of
the field. For `struct point p` at `[bp-4]` with fields `x` at
offset 0 and `y` at offset 2:

```
mov	word ptr [bp-4],10    ; p.x = 10
mov	word ptr [bp-2],20    ; p.y = 20
mov	ax,word ptr [bp-4]    ; load p.x
add	ax,word ptr [bp-2]    ; add p.y
```

So `a.x` is structurally indistinguishable from a regular int
local at the computed `bp - struct_base + field_off` offset.
The width (`word ptr` / `byte ptr`) tracks the field type.

### `->` access through a struct pointer (`105`, `106`)

For `p->x` where `p` is a pointer-to-struct, BCC emits
`[<reg> + field_off]`:

```
mov	word ptr [si],7       ; p->x = 7   (offset 0 → just [si])
mov	ax,word ptr [si]      ; ax = p->x
```

For a non-zero offset (no fixture yet, inferred):
`mov ax, word ptr [si+2]`. Identical shape to `*(p + K)` on a
non-struct pointer (fixture 091), which is reassuring — `->` is
in fact `(*p).<field>` semantically.

`&struct_var` (taking the address of a struct local) emits the
same `lea ax, word ptr [bp-N]` that any other `&local` produces.

### typedef is a pure parse-time alias (`104`)

`typedef struct { int x; } P;` followed by `P a; a.x = 7;` emits
asm **byte-identical** to the equivalent `struct s { int x; }`
form. The typedef just records the underlying type in a parser-
side alias table; no AST node escapes for it.

### Struct pointer as parameter (`106`)

A `struct s *p` parameter behaves exactly like an `int *p`
parameter: 2-byte slot at `[bp+4]`, enregisters with the same
direct-deref bonus rule (so `p->x` in the body counts as 2 uses
and easily clears the threshold).

```
_get	proc	near
	push	bp
	mov	bp,sp
	push	si
	mov	si,word ptr [bp+4]     ; receive `p` into SI
	mov	ax,word ptr [si]       ; return p->x
	…
```

## External function calls and hello-world (`096`–`100`)

### Implicit and explicit extern functions

A function called from this TU but not defined here becomes an
external symbol. BCC emits an `extrn _<name>:near` directive in the
**tail of the file**, between the final empty `_TEXT segment /
_TEXT ends` and the `public` list:

```
_TEXT	segment byte public 'CODE'
_TEXT	ends
	extrn	_puts:near        ← external reference
	public	_main
	end
```

Two source forms produce *identical* asm (fixtures 096 vs. 097):
- **Implicit declaration** (K&R-style): just `puts("hi");` with no
  prior declaration. BCC assumes `extern int puts(...)`.
- **Explicit prototype**: `int puts(char *s);` declared before
  `main`. The prototype produces no asm of its own — its only
  effect is to inform the type checker (which doesn't affect
  codegen for these simple cases).

The prototype gets parsed but doesn't enter the function-index
counter (`@N@…` labels) or the `public` list.

### String argument at a call site

When a string literal is passed as a function argument, the address
loads through AX (not the direct `mov reg, offset` used for pointer
init):

```
mov	ax,offset DGROUP:s@
push	ax
call	near ptr _puts
pop	cx
```

This is the same path used by every non-constant arg (`emit_arg_into_ax`
→ `emit_expr_to_ax`). The contrast with `char *s = "hi"` (which uses
`mov si, offset DGROUP:s@` directly) is because of *destination*: a
register-local store has a direct form, but `push ax` requires the
value in AX first.

### Multiple string literals

Each unique string literal occupies a contiguous run in `s@`:

```
_DATA	segment word public 'DATA'
s@	label	byte
	db	'a'
	db	0
	db	'b'
	db	0
_DATA	ends
```

The second literal's address is `offset DGROUP:s@+2` — the byte
position of `'b'` after the first literal's contents + NUL. Each
literal gets its own explicit `db 0` terminator (no quoted-form
NUL embedding). Identical literals deduplicate to the same offset
(no fixture pins this yet, but the natural implementation).

### Non-printable bytes in string literals

Bytes outside the ASCII printable range break out of the quoted
`db '...'` form into their own decimal `db <N>` lines. Fixture 098
(`"hi\n"`):

```
db	'hi'
db	10
db	0
```

The quoted run ends at the first non-printable, that byte gets its
own line, and a new quoted run starts after (if there's more
printable content). The closing `db 0` is the NUL terminator.

### Right-to-left arg push, with `pop cx` cleanup

Standard cdecl. For `printf("%d", 42)` (fixture 099):

```
mov	ax,42
push	ax              ; rightmost arg first
mov	ax,offset DGROUP:s@
push	ax              ; leftmost arg last
call	near ptr _printf
pop	cx              ; clean up two args
pop	cx
```

Same `pop cx` × N rule (for N ≤ 2) we've already pinned with
intra-TU calls (fixture 010).

## Pointer arithmetic and array decay (`090`–`095`)

### Array name as a value (array decay)

In C, an array name used in any non-index expression context
decays to a pointer to its first element. BCC lowers this exactly
like `&a[0]`:

```
lea	ax,word ptr [bp-N]    ; N = array's bp-offset
```

The address materializes in AX. Fixture 090 (`int *p = a;`) and
fixture 095 (`sum(a)` at a call site) both use this form. A
pointer assignment from an array decay then uses the standard
"address → AX → destination register" pattern that `&x` uses.

### Pointer dereference with offset

`*(p + K)` and `*(p + i)` are equivalent to `p[K]` / `p[i]` in
C, but BCC produces the same asm for either source form.

- **Constant offset** (`*(p + K)`, fixture 091, 094): folded to
  indexed addressing on the pointer register.
  ```
  mov	ax,word ptr [si+2]      ; *(p + 1) for int *p in SI
  mov	al,byte ptr [si+1] / cbw ; *(s + 1) for char *s
  ```
- **Variable offset** (`*(p + i)`, fixture 092): falls back to a
  load/shl/add sequence that produces the address in BX. With
  both pointer and index stack-resident:
  ```
  mov	ax,word ptr [bp-i]    ; load i
  shl	ax,1                   ; * stride
  mov	bx,word ptr [bp-p]    ; load p
  add	bx,ax                  ; bx = p + i*stride
  mov	ax,word ptr [bx]      ; *bx
  ```

### Pointer ++ / -- uses pointee-size stride

`p++` on a pointer increments by `sizeof(*p)`, and BCC emits the
stride as multiple `inc` / `dec` ops on the pointer register
(matching the `±2` int peephole pattern):

- `int *p; p++;` → `inc si / inc si` (stride 2 — fixture 090)
- `char *s; s++;` → `inc si` (stride 1 — fixture 093)

For stride > 2 BCC probably switches to `add reg, K` (same
crossover as the int +K peephole), but no fixture pins it.

### Pointer parameters

A function parameter of pointer type (`int *p`) is a 2-byte slot
on the caller-built stack. The callee's prologue receives it
exactly like an int parameter (fixture 095):

```
push	bp
mov	bp,sp
push	si
mov	si,word ptr [bp+4]    ; receive `p` into SI
```

Enregistration applies — the pointer's direct-deref use inside
the function gives it the +2 bonus, easily clearing the
threshold.

### Use-count rule refinement: direct vs. indirect deref

Pointer enregistration is gated by the same `≥ 3 uses` threshold
as ints, but **direct dereferences contribute 2 uses each** to
the pointer name:

| Form                         | Count for `p` |
|------------------------------|---------------|
| Init (`int *p = …;`)         | 1             |
| `*p` (bare deref)            | 2             |
| `*p = …;` (deref-assign)     | 2             |
| `p[K]` / `p[i]`              | 2             |
| `*(p + K)` (const offset)    | 2             |
| `*(p - K)` (const offset)    | 2             |
| `*(p + i)` (variable offset) | 1             |
| `p + i` (bare arithmetic)    | 1             |
| `p++` / `--p` / etc.         | 2             |

This explains:
- 080 (`*p`, 1 + 2 = 3): p enregisters.
- 088 (`s[0]`, 1 + 2 = 3): s enregisters.
- 091 (`*(p + 1)`, 1 + 2 = 3): p enregisters.
- 092 (`*(p + i)`, 1 + 1 = 2): p stays on stack.

The intuition: BCC's optimizer treats `*(p + <constant>)` as a
single addressed-load idiom (foldable into a single `mov reg,
[reg+K]`), so the pointer "earns" its register the same way a
direct `*p` does. A variable offset requires a runtime address
computation that uses a temp register anyway, so the pointer
doesn't pay back the register cost.

### Public-symbol list order: current approximation and open rule

We previously documented the `public` list at the end of the file as
appearing in **reverse declaration order**, which fit early fixtures only
because source order happened to match the observed output. Fixture 095
disambiguates: source order is `sum`, `main`, but the emitted list is:
```
public	_sum
public	_main
```

Alphabetically `main < sum`, so reverse alphabetical explains that
fixture and many others. Later targeted probes with multi-character
non-`main` symbols show reverse alphabetical is still not the full rule.
The current emitter matches the committed corpus with a length-bucket
approximation (longer mangled symbols first, reverse-alpha within each
bucket), but the likely real implementation is a hash-bucketed symbol
table. PUBDEF order is byte-significant in `.OBJ`, so this remains an
open compatibility question; see `specs/OPEN_QUESTIONS.md`.

## Globals and string literals (`083`–`089`)

### File layout when globals are present

Until this slice we treated `_DATA` and `_BSS` as always empty.
With globals, the file layout changes in two distinct places:

- **Initialized globals** land in a `_DATA` block at the **top** of
  the file, between the empty segment scaffold and the first
  `_TEXT segment` that holds the function code:
  ```
  _BSS	ends                       ← end of empty scaffold
  _DATA	segment word public 'DATA' ← NEW
  _<name>	label	word
  	db	<lo>
  	db	<hi>
  _DATA	ends
  _TEXT	segment byte public 'CODE' ← function code starts here
  ```
- **Uninitialized globals** land in a `_BSS` block at the **bottom**,
  between `_main endp / _TEXT ends` and the final tail. The
  function-end `?debug C E9` record *moves* from its usual spot
  (inside `_TEXT`, before `_TEXT ends`) to inside the `_BSS` block,
  right before `_BSS ends`:
  ```
  _main	endp
  _TEXT	ends                       ← _TEXT closes first
  _BSS	segment word public 'BSS'  ← NEW
  _<name>	label	word
  	db	2 dup (?)
  	?debug	C E9                  ← moved here!
  _BSS	ends
  ```
  When there are only initialized globals (no BSS content), the
  `?debug C E9` stays in its usual spot inside `_TEXT`.

### Per-global emission shape

| Type   | Init    | Anchor                  | Storage          |
|--------|---------|-------------------------|------------------|
| int    | `= K`   | `_<name> label word`    | `db <lo> / db <hi>` (little-endian byte pair) |
| char   | `= K`   | `_<name> label byte`    | `db K`           |
| int    | (none)  | `_<name> label word`    | `db 2 dup (?)`   |
| char   | (none)  | `_<name> label byte`    | `db 1 dup (?)`   |

The byte-pair `db` form for an init'd int (fixture 084: `db 42 / db
0`) is the same shape BCC uses for the linear-search switch value
table — a recurring fingerprint.

### Code references to globals

Globals are referenced as `<width> ptr DGROUP:_<name>`. The
`DGROUP:` override is mandatory; without it, the assembler would
default to `DS:` which (under the small memory model) happens to
also point to `DGROUP`, but the explicit form is what BCC always
emits.

Examples (fixture 085: write, 086: char read):
```
mov	word ptr DGROUP:_g,7        ; write to int global
mov	ax,word ptr DGROUP:_g       ; read int global
mov	al,byte ptr DGROUP:_g       ; read char global low byte
cbw	                            ; sign-extend to int
```

### Char-on-right widening dance (`087`)

When a char operand appears as the *right* side of an arithmetic op
whose left side is an int (`a + b + c` with `a, b: int` and
`c: char`), BCC can't just `add ax, byte ptr ...` — the partial
sum is in AX and the char load would clobber it. The compiler
emits a six-instruction widening dance:
```
push	ax                          ; save partial sum
mov	al,byte ptr DGROUP:_c          ; load char low byte
cbw	                              ; widen AL → AX
mov	dx,ax                          ; save widened c in dx
pop	ax                             ; restore partial sum
add	ax,dx                          ; combine
```

The same dance applies regardless of whether the char operand is a
global, a stack local, or a register local — we just don't have
fixtures for the stack-/reg-local cases yet.

### String literals

String literals live in the `s@` block of the bottom `_DATA`
section — the very label we'd been emitting as empty in every
prior fixture. The block becomes:
```
_DATA	segment word public 'DATA'
s@	label	byte
	db	'hi'                       ; literal contents
	db	0                           ; explicit NUL terminator
_DATA	ends
```

Two distinct code shapes consume a literal:

- **Address-of** (`char *s = "hi";` — fixture 088): direct
  immediate load, no AX round-trip.
  ```
  mov	si,offset DGROUP:s@
  ```
  Contrast with `&x` (a runtime address), which always goes through
  AX (`lea ax, [bp-N] / mov si, ax`). Here the literal's address is
  a linker-resolved constant, so a `mov reg, offset` is enough.

- **Constant-indexed direct** (`"hi"[0]` — fixture 089): folded at
  compile time to a direct memory reference, no register involved.
  ```
  mov	al,byte ptr DGROUP:s@
  cbw
  ```

Multi-literal programs (no fixture yet) presumably place each
literal at a `s@+<offset>` byte position. Identical literals
should dedupe to the same offset under any reasonable design;
we use a `StringPool::intern` that dedupes by content, and assume
this matches BCC.

### Public list with globals

The trailing `public` list grows to include each global, in LIFO
order of declaration over the *combined* function + global stream.
Fixture 087 (`int a; int b = 5; char c = 9; int main(...);`):
```
public _main
public _c
public _b
public _a
```

## Long and unsigned long (`200`–`286`)

A `long` is 32 bits, BCC's only multi-word integer type. Adding it
exposes a layer of codegen that doesn't appear in `int`-only code:
explicit high/low register pairs, a small set of runtime helpers in
the CRT, separate compound-assign byte-width choices for arithmetic
vs. bitwise ops, a 3-jump compare pattern, and a parameter that
occupies 4 stack bytes instead of 2. `unsigned long` shares the same
codegen except that signed-jump mnemonics for compares become
unsigned.

### Storage layout

A `long` global is 4 bytes — low word at offset `+0`, high word at
offset `+2`. Each word is little-endian internally, so byte 0 is the
LSB of the low word. An initialized `long _g = 5` emits four `db`
bytes (`db 5 / db 0 / db 0 / db 0`), one per byte of the 32-bit
value. An uninitialized long takes `db 4 dup (?)` in `_BSS`. Stack
locals and parameters follow the same word-pair convention:
`long x; x` is split across `[bp-N]` (low) and `[bp-N+2]` (high) for
a stack local at offset N. For a parameter, low is at `[bp+M]` and
high at `[bp+M+2]`.

### Register conventions are context-dependent

BCC uses *two different* register-pair assignments for the high and
low halves of a long, depending on where the value lives:

| Context                      | High in  | Low in  |
|------------------------------|----------|---------|
| Globals: arithmetic load     | **AX**   | **DX**  |
| Stack/params: arithmetic load| **DX**   | **AX**  |
| Function return / helper ABI | **DX**   | **AX**  |

In all contexts, BCC loads the **high half first**, then the low
half:
- Global: `mov ax, _g+2` then `mov dx, _g`.
- Stack: `mov dx, [bp+6]` then `mov ax, [bp+4]`.

The arithmetic op then runs on the low half (`add dx, …` for globals;
`add ax, …` for stack), and `adc` propagates carry into the high half.

Return-value convention follows the standard 16-bit cdecl: `DX=high,
AX=low`. `return 5L` emits `xor dx,dx / mov ax,5` (fixture 212). A
`return long_param + long_param` keeps the result in DX:AX from the
final `adc` (fixture 285).

### Return and constant load

`return <const>` for a long materializes the high and low halves
separately:

```
xor dx, dx        ; high = 0
mov ax, 5         ; low = 5
jmp short @1@50   ; standard exit
```

For small positive constants the high is `xor dx,dx`; for negative
constants both halves get explicit `mov` of the sign-extended value.
`return <ident>` for a long parameter or global loads both halves
into DX:AX:

```
mov dx, word ptr [bp+6]   ; high
mov ax, word ptr [bp+4]   ; low
```

The single `jmp short @<f>@50` epilogue is the same as for ints —
there is no separate long-return exit shape.

### Plain assignment and arithmetic

`g = K` for a long global stores high then low:

```
mov word ptr DGROUP:_g+2, 0       ; high = 0
mov word ptr DGROUP:_g, 5         ; low = 5
```

The high is stored first when the value is a constant — for
register-resident values the order can flip (see below).

`g = a + b` with both operands long globals routes through AX:DX
using the global convention (AX=high, DX=low):

```
mov ax, word ptr DGROUP:_a+2     ; AX = a.high
mov dx, word ptr DGROUP:_a       ; DX = a.low
add dx, word ptr DGROUP:_b       ; DX += b.low
adc ax, word ptr DGROUP:_b+2     ; AX += b.high + carry
mov word ptr DGROUP:_g+2, ax     ; store high
mov word ptr DGROUP:_g, dx       ; store low
```

The same shape covers `g = a - b` (sub/sbb), `g = a + 10` (add
imm/adc 0), and `g = i + g` commuted (loads from globals as DX:AX-on-
memory rather than BX:CX/DX:AX — see slice 281).

`return a + b` for two long parameters uses the stack convention
(DX=high, AX=low) — see fixture 285's six instructions above.

### Compound assignment: arithmetic uses `83` (imm8sx), bitwise uses `81` (imm16)

For longs, BCC emits a **memory-direct read-modify-write** pair (no
DX:AX round-trip) for both arithmetic and bitwise compound assigns,
but the immediate-width selection is op-family dependent. The rule
holds for both globals and stack locals — only the ModR/M byte
differs.

Arithmetic (`+=` / `-=`), global:

```
add word ptr DGROUP:_g, 5      ; 83 06 lo hi 05      (5 bytes, imm8sx)
adc word ptr DGROUP:_g+2, 0    ; 83 16 lo hi 00      (5 bytes, imm8sx)
```

Bitwise (`&=` / `|=` / `^=`), global:

```
and word ptr DGROUP:_g, 15     ; 81 26 lo hi 0F 00   (6 bytes, imm16)
and word ptr DGROUP:_g+2, 0    ; 81 26 lo hi 00 00   (6 bytes, imm16)
```

Same code shape applies to long *stack locals* — the ModR/M r/m field
switches from mem16 (`r/m=110` with disp16) to mem8 (`r/m=110` with
disp8), shrinking each half by one byte:

```
; long x; x += 10;    (x at [bp-4])
add word ptr [bp-4], 10        ; 83 46 fc 0A          (4 bytes, imm8sx)
adc word ptr [bp-2], 0         ; 83 56 fe 00          (4 bytes, imm8sx)

; long x; x &= 7;
and word ptr [bp-4], 7         ; 81 66 fc 07 00       (5 bytes, imm16)
and word ptr [bp-2], 0         ; 81 66 fe 00 00       (5 bytes, imm16)
```

The ModR/M byte cycles through the Grp1 `/n` field: `/0` ADD (`46`),
`/1` OR (`4E`), `/2` ADC (`56`), `/3` SBB (`5E`), `/4` AND (`66`),
`/5` SUB (`6E`), `/6` XOR (`76`). Same /n choices as for globals
(`06`/`0E`/`16`/`1E`/`26`/`2E`/`36`), only the `r/m` and addressing
mode differ.

For bitwise compound the high partner is `op <mem>, <high K>` rather
than the carry-propagating `adc/sbb 0` — bitwise ops don't carry.
When K fits in i8sx, the bitwise path still picks the wider encoding
(1 byte wasted per half). This *isn't* a TASM default — TASM's
sign-extension heuristic would pick `83` for `15`. BCC's emitter
must specifically request `81` for the bitwise compound shapes,
regardless of storage class.

For `*=` / `/=` / `%=`, the compound form routes through the helper
ABI (see "Runtime helpers" below) since BCC has no memory-direct
mul/div path. Same for shifts (see "Long compound shift" below).

### `g = g op K` vs `g op= K`

Just as for ints, `g = g + K` and `g += K` produce *different* asm
even though they are semantically identical. The plain-assign form
routes through AX:DX:

```
mov ax, _g+2 / mov dx, _g / add dx, 10 / adc ax, 0 / mov _g+2, ax / mov _g, dx
```

The compound form uses the memory-direct `83 06`/`83 16` pair. Both
forms preserve the BCC compound-vs-plain fingerprint into long
arithmetic.

### `g = g * 2` peephole: `shl/rcl`

`g = g * 2` on a long global skips the multiply helper and folds to
a left-shift through the carry chain:

```
mov ax, word ptr DGROUP:_g+2
mov dx, word ptr DGROUP:_g
shl dx, 1
rcl ax, 1
mov word ptr DGROUP:_g+2, ax
mov word ptr DGROUP:_g, dx
```

`shl dx, 1` shifts the low half left by 1; the bit shifted out goes
to CF. `rcl ax, 1` rotates the high half left through CF, picking up
that bit. Two instructions instead of an `N_LXMUL@` call. Likely
extends to any constant doubling that fits in `shl/rcl` repeats,
though no fixture pins K=4 etc. _Fixture_: 283.

### Long compound shift reorders `mov cl, K` first

For `long a <<= 2`, the shift count establishes CL **before** the
helper inputs are loaded:

```
mov cl, 2
mov dx, word ptr DGROUP:_a+2
mov ax, word ptr DGROUP:_a
call near ptr N_LXLSH@
mov word ptr DGROUP:_a+2, dx
mov word ptr DGROUP:_a, ax
```

Compare with the plain-assign form `a = a << K`, where `mov cl, K`
comes *after* loading DX:AX. The reorder is small but distinguishing
— our codegen must special-case the compound-shift K>1 path. K=1
folds to `shl/rcl` (no helper, no CL load) — same as the `*2`
peephole.

### Runtime helpers

Long multiply, divide, modulo, and variable-count shifts route through
helpers in the Borland CRT:

| Helper        | Operation                       |
|---------------|---------------------------------|
| `N_LXMUL@`    | signed/unsigned long multiply   |
| `N_LDIV@`     | signed long divide              |
| `N_LMOD@`     | signed long modulo              |
| `N_LUDIV@`    | unsigned long divide            |
| `N_LUMOD@`    | unsigned long modulo            |
| `N_LXLSH@`    | long shift-left (CL count)      |
| `N_LXRSH@`    | signed long shift-right         |
| `N_LXURSH@`   | unsigned long shift-right       |

Calling convention varies by helper:

- **`N_LXMUL@`**: both operands passed as DX:AX pairs in registers
  (not pushed). Result in DX:AX.
- **`N_LDIV@` and family**: both operands pushed on the stack (high
  first, then low — same as a normal `long` arg pair). Quotient or
  remainder returned in DX:AX. **No caller stack cleanup** — the
  helper pops its own 8 bytes of arguments (`ret 8`-style).
- **`N_LXLSH@`/`N_LXRSH@`/`N_LXURSH@`**: shift count in CL, value in
  DX:AX. Result in DX:AX.

Helper extern declarations use `:far` (BCC's user-function externs
use `:near`):

```
extrn N_LXLSH@:far
```

The helper extern is *not* segregated into its own block — it joins
the publics list at end-of-file, participating in the same length-
bucket + reverse-alphabetical sort used for ordinary publics. A
typical `<<= 2` tail:

```
public _main
extrn N_LXLSH@:far
public _a
```

_Fixtures_: 232 (`N_LDIV@`), 245 (`N_LUDIV@`), 247 (`N_LXMUL@`), 263
(`N_LXLSH@`).

### Long compares: zero, vs-const, vs-long

`if (g == 0)` for a long collapses both halves into AX via OR:

```
mov ax, _g
or  ax, _g+2          ; ZF set iff both halves were 0
jne short @<skip>
```

The OR sets ZF=1 iff both source halves were 0 (equivalent to
testing whether the full 32-bit value is 0). Used for `g == 0`,
`g != 0`, and bare-`if (g)` truth tests. _Fixtures_: 215, 238.

`if (a < b)` for two long globals lowers to a high-half compare with
3 possible outcomes, dispatched with a 3-jump pattern:

```
mov ax, _a+2
mov dx, _a
cmp ax, _b+2          ; high halves
jg  short @false      ; signed: a.high > b.high → not less
jl  short @true       ; signed: a.high < b.high → less
cmp dx, _b            ; equal high → low halves
jae short @false      ; UNSIGNED: a.low >= b.low → not less
@true:
```

Three observations:

1. The high comparison uses **signed** mnemonics (`jg`/`jl`); the low
   uses **unsigned** (`jae`/`jb`/`ja`/`jbe`). For `unsigned long`,
   both halves use unsigned.
2. The mnemonics flip per operator. For `<=` the low-half test is
   `ja @false`; for `>` it's `jbe @false`; etc. There is no compact
   table — each of `<`/`<=`/`>`/`>=` picks its own jump triple.
3. `==`/`!=` use a different shape: combine OR pattern for `== 0`,
   or chained cmp + je for `== K`.

For long-vs-int compares (mixed types, slice 273), BCC swaps the
operand order to keep the long on the LHS and widens the int via
`cwd` before the compare. The 3-jump pattern is otherwise the same.
_Fixtures_: 234–242, 273.

### Long parameters and arguments

A `long` argument is pushed as two `push <reg>` instructions, **high
half first**, so the low half ends up at the lower bp-offset in the
callee. A constant `long 5` is materialized as:

```
xor ax, ax        ; AX = high (0)
mov dx, 5         ; DX = low (5)
push ax           ; push high first
push dx           ; push low last (now at lower offset)
call near ptr _f
pop cx
pop cx            ; cleanup: 2 pops, not 1
```

A long parameter takes **4 stack bytes**, not 2. For `long f(long a,
long b)`:

- `a.low` at `[bp+4]`, `a.high` at `[bp+6]`
- `b.low` at `[bp+8]`, `b.high` at `[bp+10]`

This means the bp-offset for the second parameter depends on the
first parameter's width — see `crates/bcc/src/codegen/locals.rs` for
the per-type advance: 4 bytes for `long`/`unsigned long`, 2 for
everything else.

The caller-side cleanup rule (`pop cx` × N for N ≤ 2 args, `add sp,
N*2` for N ≥ 3) treats each pushed word as one "arg" for the
threshold — a single long arg counts as 2, and a single-long call
emits 2 `pop cx`. Two long args (4 pushes) flips to `add sp, 8`.
_Fixtures_: 216 (`f(5L)` → push/push/pop/pop), 217 (`long f(long x)`
reads `[bp+4]`/`[bp+6]`), 285 (two long params, second at `[bp+8]`).

#### Non-constant long arguments (`322`–`328`)

When a long argument is a non-constant lvalue (global, local, deref,
struct field, array element), BCC pushes both halves **memory-direct**
using `FF /6` Grp5 push variants — no `mov` to a register first.
The high half pushes first, low half second (same order as the
constant-arg path):

| Source                       | High push                          | Low push                          |
|------------------------------|------------------------------------|-----------------------------------|
| Long global `g`              | `push word ptr DGROUP:_g+2`        | `push word ptr DGROUP:_g`         |
| Long stack local `y`         | `push word ptr [bp+off+2]`         | `push word ptr [bp+off]`          |
| `*p` (long pointer in `si`)  | `push word ptr [si+2]`             | `push word ptr [si]`              |
| `s.x` (long field at off N)  | `push word ptr DGROUP:_s+N+2`      | `push word ptr DGROUP:_s+N`       |
| `a[K]` (const index in long array) | `push word ptr DGROUP:_a+K*4+2` | `push word ptr DGROUP:_a+K*4`     |

Encodings — every push opcode is `FF` plus a Grp5 `/6` ModR/M byte:

```
FF 36 lo hi       push word ptr DGROUP:_g                (4 bytes, FIXUPP-patched)
FF 76 dd          push word ptr [bp+disp8]               (3 bytes)
FF 34             push word ptr [si]                     (2 bytes, disp-less form)
FF 74 dd          push word ptr [si+disp8]               (3 bytes)
```

Notable: the low-half push from `[si]` uses the **disp-less**
`FF 34` form (2 bytes), while the high-half push from `[si+2]`
uses `FF 74 02` (3 bytes). Same disp-less shortcut as the
corresponding `mov dx, word ptr [si]` (`8B 14`) load.

Mixed int+long argument lists push right-to-left as expected, each
in its own type's shape — an int arg materializes into AX and emits
`push ax`, while a long arg uses the two-push memory-direct shape.
Fixture 327 (`f(7, g)` with `int a, long b`):

```
push word ptr DGROUP:_g+2     ; rightmost arg first, high half
push word ptr DGROUP:_g       ; low half
mov  ax, 7                    ; leftmost arg
push ax
call near ptr _f
add  sp, 6                    ; 3 words = 6 bytes, ≥3 → add sp
```

The cleanup `add sp, N` is driven purely by total pushed bytes,
agnostic to which words came from int args vs long args.

### Widening and narrowing

`long g = i` (int → long) widens via `cwd`:

```
mov ax, word ptr DGROUP:_i      ; AX = int
cwd                              ; DX:AX = sign-extended int (DX=high)
mov word ptr DGROUP:_g+2, dx
mov word ptr DGROUP:_g, ax
```

`long g = u` (unsigned int → unsigned long) zero-extends with a
direct store:

```
mov ax, word ptr DGROUP:_u
xor dx, dx                       ; or: mov word ptr DGROUP:_g+2, 0
mov word ptr DGROUP:_g+2, dx
mov word ptr DGROUP:_g, ax
```

`long g = c` (char → long) does `cbw / cwd` to chain the two
widenings:

```
mov al, byte ptr DGROUP:_c
cbw                              ; AL → AX (sign-extend)
cwd                              ; AX → DX:AX (sign-extend)
mov word ptr DGROUP:_g+2, dx
mov word ptr DGROUP:_g, ax
```

`int g = (int)long_g` (long → int) just drops the high half:

```
mov ax, word ptr DGROUP:_lg     ; AX = low half only
mov word ptr DGROUP:_g, ax       ; store as int
```

`unsigned long ↔ long` conversions are no-ops at the asm level — the
bit pattern is identical and BCC trusts the type system to surface
sign-vs-unsigned only at compare/divide time. _Fixtures_: 254, 255,
256, 271, 272, 279, 277, 278.

### Unsigned long

`unsigned long` shares all the codegen above except for:

- Compares: both halves use unsigned mnemonics (`jb`/`ja`/`jae`/`jbe`),
  not the mixed signed-high/unsigned-low.
- Divide/modulo route to `N_LUDIV@`/`N_LUMOD@`, not `N_LDIV@`/`N_LMOD@`.
- Right shift routes to `N_LXURSH@`, not `N_LXRSH@`.
- Widening from `unsigned int` zero-extends (direct `xor dx, dx`)
  rather than `cwd`-sign-extending.

_Fixtures_: 242 (unsigned long compare), 243 (`>>` peephole),
244–247 (helpers), 277/278 (cast).

### Long arrays

Long arrays follow the same low/high word storage convention as scalar
longs: each element occupies 4 bytes, low word at offset +0 of the
element, high word at +2. Layout differs slightly by storage class.

**Initialized global** (`long a[3] = {1, 2, 3};`) — one 4-byte LE pair
per element, dropped into `_DATA` via the per-byte `db` idiom:

```
_a   label word
     db 1 / db 0 / db 0 / db 0       ; a[0] = 1
     db 2 / db 0 / db 0 / db 0       ; a[1] = 2
     db 3 / db 0 / db 0 / db 0       ; a[2] = 3
```

**Uninitialized global** (`long a[3];`) — `db 12 dup (?)` in `_BSS`
(reservation = `sizeof(elem) * count`).

**Stack-local** (`long a[2];`) — frame grows by `4 * count` bytes. The
ordering matches scalar locals: element 0 sits at the most-negative
bp-offset, with elements climbing toward `bp`. Within each element,
low at +0, high at +2.

```
sub sp, 8                            ; long a[2]; → 8-byte frame
mov word ptr [bp-6], 0               ; a[0].high
mov word ptr [bp-8], 5               ; a[0].low  = 5
mov word ptr [bp-2], 0               ; a[1].high
mov word ptr [bp-4], 7               ; a[1].low  = 7
```

### Long array element access — constant index

A constant index folds to a direct memory operand at the
element's byte offset. Read of `g = a[1]` for a global array:

```
mov ax, word ptr DGROUP:_a+6         ; high of a[1] (byte 6)
mov dx, word ptr DGROUP:_a+4         ; low of a[1]  (byte 4)
mov word ptr DGROUP:_g+2, ax         ; store high
mov word ptr DGROUP:_g, dx           ; store low
```

For stack arrays, the same shape uses bp-relative addressing:

```
mov ax, word ptr [bp-2]              ; high of a[1] (local at bp-4)
mov dx, word ptr [bp-4]              ; low of a[1]
```

Const-index *writes* drop the load: `a[1] = 42` becomes two
`mov word ptr <addr>, K` calls — high half first, then low. The same
high-first ordering as scalar long assigns.

### Long array element access — variable index

For globals, variable-indexed access uses **bx-indexed addressing
with the symbol folded into the disp16**, *not* the 4-instruction
stack-array effective-address pattern. The index is loaded into BX,
scaled by 4 with two `shl bx, 1` (stride 4 = 2² — BCC stays on
8086-compatible single-bit shifts), then both halves read via
`<sym>[bx+disp]` where `disp` is 0 for low and 2 for high:

```
mov bx, word ptr [bp-2]              ; bx = i
shl bx, 1
shl bx, 1                            ; bx = i*4 (long stride)
mov ax, word ptr DGROUP:_a[bx+2]     ; high of a[i]: 8B 87 lo hi + FIXUPP
mov dx, word ptr DGROUP:_a[bx]       ; low  of a[i]: 8B 97 lo hi + FIXUPP
mov word ptr DGROUP:_g+2, ax
mov word ptr DGROUP:_g, dx
```

The disp16 bytes encoded into the instruction are the byte offset
**within** the element (`02 00` for high, `00 00` for low); FIXUPP
patches the symbol's segment-relative location *additively* so the
final effective address is `<sym>+<elem-offset>+bx`.

Variable-indexed *writes* parallel: `mov word ptr DGROUP:_a[bx+2], hi`
/ `mov word ptr DGROUP:_a[bx], lo` (`C7 87 ...` encoding).

### Index-into-BX is three shapes

BCC picks the shortest of three forms for the BX load:

- **Int stack local**: `mov bx, word ptr [bp-N]` — 3 bytes.
- **Int register local**: `mov bx, <reg>` — 2 bytes (register-to-
  register). Common in loops where the index is the loop variable
  and clears the use-count threshold.
- **Anything else** (constant expression unfolded, arithmetic, etc.):
  compute into AX (the usual `emit_expr_to_ax` path), then
  `mov bx, ax` — 2 bytes for the move plus whatever the compute cost.

After the BX load, the two stride shifts are unconditional for `long`:
`shl bx, 1 / shl bx, 1` regardless of whether the index sat in a
register or on the stack. _Fixtures_: 303 (stack-int i), 305
(stack-int i with write), 307 (register-int i in a `while` loop).

### Stack-array vs global-array variable index differ in shape

Worth noting that the global-array variable-index form is genuinely
different from the stack-array effective-address compute documented
earlier in this file. For stack arrays we have:

```
mov bx, <index>           ; or `mov bx, [bp-N]`
shl bx, 1                 ; scale (× stride / 2)
lea ax, word ptr [bp-N]   ; base address
add bx, ax                ; bx = element address
mov ax, word ptr [bx]     ; deref
```

For global arrays we have:

```
mov bx, <index>           ; same load shapes
shl bx, 1
shl bx, 1                 ; (×4 for long)
mov ax, word ptr <sym>[bx+disp]   ; one instruction, FIXUPP folds <sym> in
```

The global form is 4 bytes shorter because the linker resolves the
base symbol into the disp16 slot at link time, removing the runtime
`lea + add bx, ax`. _Fixtures_: 303 vs 079 (stack int array variable
index); 305 vs 184 (variable-indexed stack array write).

### Long pointers (`308`–`313`)

A `long *` is a 2-byte near pointer like every other pointer in the
small model, but the *pointee* is 4 bytes — so deref operations
expand to two-word memory accesses (high first, then low — same
order as scalar long stores).

#### Pointer init from a global address

`long *p = &g;` and `long *p = a;` (array decay) both materialize the
linker-resolved offset directly into the destination register, with
no AX round-trip:

```
mov si, offset DGROUP:_g          ; long *p = &g;
mov si, offset DGROUP:_a          ; long *p = a;  (array decay)
```

Distinct from `&<stack-local>` (which always routes through AX via
`lea ax, word ptr [bp-N] / mov <reg>, ax`) because globals/arrays are
link-time constants, not runtime addresses. The same shortcut already
applies to string-literal initializers (fixture 088).

#### Deref read: `g = *p`

Loads high then low into the AX:DX globals-convention pair:

```
mov ax, word ptr [si+2]           ; high   — 8B 44 02
mov dx, word ptr [si]             ; low    — 8B 14
mov word ptr DGROUP:_g+2, ax      ; store high
mov word ptr DGROUP:_g, dx        ; store low
```

The low-half load uses the **disp-less** `8B 14` form (ModR/M 14 =
mod=00 reg=DX r/m=100 [si]), 2 bytes, while the high half uses the
`8B 44 02` form with disp8 (3 bytes). The high-first load order is
the same as for long globals and long stack locals — see "Long load
order" — but the register convention here is the **ABI** layout
(DX=high, AX=low) rather than the globals-arithmetic layout
(AX=high, DX=low). A bare long-pointer deref reads into ABI registers
because the result is treated as a "long value", not as the operand
of a global-arithmetic chain.

#### Deref write: `*p = K`

Stores high then low. BCC writes the constant 0 to the high half
first, then the actual low-half value:

```
mov word ptr [si+2], 0            ; high  — C7 44 02 00 00
mov word ptr [si], 42             ; low   — C7 04 2A 00
```

The low-half store uses `C7 04 lo hi` (disp-less); the high uses
`C7 44 02 lo hi` (disp8). Order matches the high-first rule for all
long stores.

#### Compound assign: `*p += K`

Memory-direct read-modify-write, same op-family byte-width rule as
for long globals and long stack locals:

```
add word ptr [si], 5              ; 83 04 05  (low,  3 bytes, imm8sx)
adc word ptr [si+2], 0            ; 83 54 02 00 (high carry, 4 bytes)
```

The op-family selection (arith uses `83`, bitwise uses `81`) is the
same across all three storage classes (global, stack-local, pointer-
target). The ModR/M `r/m` field changes per addressing mode but the
opcode byte and immediate-width choice don't.

#### Long pointer parameters

A `long *` parameter takes 2 stack bytes (it's a 2-byte pointer, not a
4-byte long). The direct-deref bonus (see fingerprints) gives `*p`
two use counts toward enregistration, so any function that derefs its
long pointer parameter once already clears the int 3-use threshold:

```
_f      proc near
        push bp
        mov  bp, sp
        push si
        mov  si, word ptr [bp+4]   ; receive `p` into SI
        mov  word ptr [si+2], 0    ; *p = K — through SI
        mov  word ptr [si], 99
```

Identical prologue shape to an `int *` parameter; only the deref
expands to two words instead of one.

#### Indexed access: `p[K]` for register-resident pointer

For a register-resident long pointer, `p[K]` lowers to the same
memory-direct shape as `*(p + K)` — no separate array-style
effective-address compute. Constant K folds the byte-offset into the
ModR/M disp:

```
p[0] = 42:                         p[1] = 42:
  mov word ptr [si+2], 0             mov word ptr [si+6], 0
  mov word ptr [si], 42              mov word ptr [si+4], 42
```

This is the same idiom documented earlier as `*(p + K)` folding to
indexed addressing — the source forms `p[K]`, `*(p + K)`, and `*p`
(when K=0) all produce identical asm.

#### Stride 4 → `add reg, K` peephole

The pointer ±K peephole inherited from int pointers crosses to the
`add reg, K` form **at stride 4**:

- `char *s; s++;` → `inc si` (1 byte; stride 1).
- `int *p; p++;` → `inc si / inc si` (2 bytes; stride 2 — 1 byte
  cheaper than `add si, 2` at 3).
- `long *p; p++;` → `add si, 4` (3 bytes; four `inc`s would cost 4).

So the crossover sits between stride 2 and stride 3+ — same as the
int compound `x += K` peephole, applied to pointer arithmetic.
_Fixture_: 313.

### Long function-call returns (`314`, `315`, `321`)

A function with `long` return type returns its value in `DX:AX` (the
standard 16-bit cdecl long ABI — DX=high, AX=low). The caller-side
shape for receiving the value mirrors any other long write: store
DX → high half, AX → low half, at whatever destination address the
target requires.

**Into a long global** (fixture 314, `g = f();`):

```
call near ptr _f
mov  word ptr DGROUP:_g+2, dx
mov  word ptr DGROUP:_g,   ax
```

**Into a long stack local** (fixture 315 init / 321 assign):

```
call near ptr _f
mov  word ptr [bp-2], dx        ; high
mov  word ptr [bp-4], ax        ; low
```

The init form (`long x = f();`) and assign form (`long x; x = f();`)
produce identical bytes — the only difference upstream is whether
the locals planner has already allocated `x`'s frame slot at the
declaration site. Either way the call result lands directly in DX:AX
and BCC's emitter stores both halves with non-AX `mov reg → mem`
encodings (`89 56 fe` for `mov [bp-2], dx`, `89 46 fc` for `mov
[bp-4], ax`).

The order is **DX first, then AX** — same "high first, then low"
rule that holds for every other long memory store. The caller never
spills DX:AX into AX:DX (the globals-arithmetic convention) for a
plain assign — that swap only happens when the long flows into a
register-arithmetic computation.

### Long struct fields (`316`–`320`)

Long-typed struct fields share the same memory layout as scalar
longs: low word at field-base offset, high word at field-base + 2.
BCC's struct packing (no inter-field padding — see "Packed structs"
in fingerprints) means the field's byte offset is exactly the sum
of preceding field sizes, regardless of alignment requirements.

For `struct S { int a; long x; }` global `s`, the layout is:

```
_s     label word
       db ...     ; a at +0 (2 bytes)
       db ...     ; x.low at +2 (2 bytes)
       db ...     ; x.high at +4 (2 bytes)
```

So `s.x` low sits at `DGROUP:_s+2` and high at `DGROUP:_s+4`, *not*
the +4/+6 a padded compiler would produce. _Fixture_: 320.

**Long field write**, dot access on a global struct (fixture 316):

```
mov word ptr DGROUP:_s+2, 0       ; high (field x at offset 0 here)
mov word ptr DGROUP:_s,   5       ; low
```

**Long field read**, dot access on a global struct (fixture 317,
`g = s.x;`):

```
mov ax, word ptr DGROUP:_s+2      ; high
mov dx, word ptr DGROUP:_s        ; low
mov word ptr DGROUP:_g+2, ax      ; store high to g
mov word ptr DGROUP:_g,   dx      ; store low
```

The pattern is mechanically the long-global-to-long-global copy
with field offset folded into the addresses.

**Long field on stack struct** (fixture 319) — same shape but the
addresses are `[bp+off]` / `[bp+off+2]`:

```
mov word ptr [bp-2], 0            ; s.x.high
mov word ptr [bp-4], 99           ; s.x.low
```

**Long field via struct pointer** (fixture 318, `p->x = 7;` where
`p: struct S *` in SI):

```
mov word ptr [si+2], 0            ; high — same as raw long-ptr deref
mov word ptr [si],   7            ; low
```

For a field at offset N within the struct, the addresses become
`[si+N]` and `[si+N+2]`. `p->x` for the first field (offset 0) is
byte-identical to `*p` for a long pointer.

## Source-line comments

BCC interleaves the source as comments. Observed layout for `004`:

```
   ;	
   ;	int main(void) {
   ;	
	push	bp
	... prologue ...
   ;	
   ;	  int x = 5;
   ;	
	mov	word ptr [bp-2],5
   ;	
   ;	  return x;
   ;	
	mov	ax,word ptr [bp-2]
	jmp	short @1@50
@1@50:
   ;	
   ;	}
   ;	
	mov	sp,bp
	... epilogue ...
```

Three observations:

1. Each comment block is **three lines**: an empty `   ;\t`, the source
   line, and another empty `   ;\t`. (For statements on the same source
   line in `001`/`003`, the whole `int main(void) { return 0; }` shows up
   as one inner line.)
2. The opening brace `{` and the closing brace `}` each get their own
   comment block, attached to the prologue/epilogue respectively.
3. The leading whitespace on the source line is preserved verbatim — the
   `  int x = 5;` retains its two leading spaces from the C source.

## Open questions (track for future fixtures)

- Public/PUBDEF ordering general rule. The current emitter uses a
  length-bucket + reverse-alpha approximation that fits committed
  fixtures, but targeted probes suggest a hash-bucketed symbol table.
- `@<func>@C<num>` data-label `<num>` formula. Jump-table fixtures
  073 (n=8, C1244) and 076 (n=4, C876) both fit `92·n + 508`;
  linear-search fixture 074 (n=4, C738) fits `74·n + 442`. These
  empirical fits match every data point we have, but the constants
  `92`, `508`, `74`, `442` have no obvious source — they're not
  byte offsets within the function, not derivable from slot
  numbering, and the choice of multiplier differs between
  strategies. Capture a fixture with a different function/TU
  shape (e.g. multiple constants, a switch later in the function,
  two switches) and see whether the same formula still holds.
- Are `d@`/`d@w` and `b@`/`b@w` ever positioned mid-segment, or always
  at the segment head?
- Two-operand `imul` (80186/286): does any `-mc`/`-ml` model or higher
  target switch BCC to it?
- What does `-O`/`-G`/`-r`/`-Z` actually change in the output? We've
  only run with `-ms`.
