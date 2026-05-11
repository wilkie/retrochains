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

### Calling a function (`010`)

```
	call	near ptr _f
```

- Small-memory-model: all calls are **near**, but BCC writes
  `near ptr` explicitly (TASM accepts both with and without; the explicit
  form is the bytes BCC produces).
- Calling convention is cdecl-like: caller pushes args right-to-left
  (we haven't seen args yet — fixture 010 is no-arg). Return value lives
  in AX.
- For `return f();`, no setup is needed: the result is already in AX
  after the `call`, then the standard `jmp short @<f>@50` to the exit.

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
