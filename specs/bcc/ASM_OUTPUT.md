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

```
_TEXT	segment byte public 'CODE'
   ;	
   ;	int main(void) { return 0; }
   ;	
	assume	cs:_TEXT
_main	proc	near
	push	bp
	mov	bp,sp
	... body ...
	jmp	short @1@50
@1@50:
	... epilogue ...
	ret	
_main	endp
	?debug	C E9
_TEXT	ends
```

- Source-line comments are emitted as `   ;\t<source-text>\r\n`, with each
  statement bracketed by an empty `   ;\t` line before and after the next
  comment block.
- C function name `main` becomes ASM symbol `_main` (leading underscore is
  the standard Borland small-model convention).
- Every function has a **single exit label**: `@1@50` in fixture 001.
  Even an unconditional return goes via `jmp short @1@50` to that label,
  which holds the epilogue. The `1` is presumably the function index and
  `50` is its exit-label number; we've only seen function 1 so far.
- The function ends with `?debug C E9` — a debug-comment record with just
  the type byte and no payload, presumably marking end-of-function.

### Tail (constant across the three `-S` fixtures we've captured)

```
_DATA	segment word public 'DATA'
s@	label	byte
_DATA	ends
_TEXT	segment byte public 'CODE'
_TEXT	ends
	public	_main
	end
```

`s@` is another section-base label, this time for strings/static data
(unused by these fixtures). The `_TEXT` is re-opened and re-closed in case
later sections need it. `public _main` exports the function, and `end`
closes the assembly file.

## Codegen patterns observed

### Return constant

| C source       | Asm                | Notes                                  |
| -------------- | ------------------ | -------------------------------------- |
| `return 0;`    | `xor ax,ax`        | Smaller encoding than `mov ax,0`       |
| `return 42;`   | `mov ax,42`        | Decimal in source; no `0x`/`h` suffix  |

Both are followed by `jmp short @1@50` to the unified exit, never an
inline `ret`.

### Stack frame

- **No locals (`001`, `003`)**: `push bp` / `mov bp,sp` ... `pop bp` /
  `ret`.
- **One `int` local (`004`)**: `push bp` / `mov bp,sp` / **`dec sp` /
  `dec sp`** (two single-byte decrements rather than `sub sp,2` — saves a
  byte) ... `mov sp,bp` / `pop bp` / `ret`. The local is at `[bp-2]`.

### Local-variable access

```
	mov	word ptr [bp-2],5         ; int x = 5;
	mov	ax,word ptr [bp-2]        ; return x;
```

`word ptr` is always written explicitly even when the destination is a
16-bit register that would otherwise disambiguate the size.

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

- What's the `@<n>@<m>` label scheme? `@1@50` is constant across our
  three fixtures. Does `@1` step to `@2` for a second function?
  Does `@50` step for additional labels (else-branches, loops)?
- Does the `s@` label ever become non-empty? Probably for string literals.
- Are `d@`/`d@w` and `b@`/`b@w` ever positioned mid-segment, or always
  at the segment head?
- What does `-O`/`-G`/`-r`/`-Z` actually change in the output? We've
  only run with `-ms`.
