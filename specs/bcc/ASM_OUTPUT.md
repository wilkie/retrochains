# BCC `-S` assembly output format

What `BCC.EXE -S <source>.C` writes to `<source>.ASM`. All observations are
drawn from fixtures `001-empty-main`, `003-return-constant`, and
`004-int-variable`; cite the fixture each time a claim is extended.

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

`s@` is another section-base label, this time for strings/static data
(unused by our `-S`-only fixtures so far). The `_TEXT` is re-opened and
re-closed in case later sections need it.

The `public _<sym>` lines appear **in reverse definition order**.
Fixtures 009 and 010 both define `f` first and `main` second, but emit
`public _main` before `public _f`. Probably an artifact of how BCC walks
its internal symbol table (LIFO insertion); for byte-exactness we need to
match it.

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

1. **`x + 1` on a value already in AX becomes `inc ax`**, not
   `add ax,1`. This is a constant-rhs peephole we haven't seen before.
   (Probably symmetric `dec ax` for `x - 1`; needs a fixture.)
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

So the **leftmost** parameter sits closest to `bp`. Every `int` arg
takes a 2-byte slot regardless of declared type — `char` arguments
would presumably be promoted at the caller's push site to a 2-byte
push (we don't have a fixture confirming this).

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

- `@<n>@<m>` label scheme: `@n` steps per function (confirmed). `@50`
  is the exit label number — does it step for additional labels
  (else-branches, loops, gotos)? Probably 50 is just "the exit
  label slot" and other labels get @51, @52, …
- Why does `public` ordering appear to be LIFO over the symbol table?
  When we add globals and externs, find out where they slot in.
- Does the `s@` label ever become non-empty? Probably for string literals.
- Are `d@`/`d@w` and `b@`/`b@w` ever positioned mid-segment, or always
  at the segment head?
- 3-byte stack frame: 3× `dec sp` or `sub sp,3`? (Pin down the
  `dec`→`sub` crossover.)
- Two-operand `imul` (80186/286): does any `-mc`/`-ml` model or higher
  target switch BCC to it?
- Call with arguments: cdecl push-and-pop pattern.
- What does `-O`/`-G`/`-r`/`-Z` actually change in the output? We've
  only run with `-ms`.
