# ASM — TASM-flavored MASM text assembly

The on-disk format `BCC -S` produces and `TASM` consumes on its way
to OMF. A plain-text file (CRLF line-endings, DOS conventions), but
with framing rules tight enough that "byte-exact" is a meaningful
goal.

The spec source is Borland's TASM 2.0 manual plus the
Borland-specific conventions observed across fixtures `001-103`.
For the BCC-specific patterns that *fill in* this envelope —
prologues, codegen idioms, switch dispatch — see
[`../bcc/ASM_OUTPUT.md`](../bcc/ASM_OUTPUT.md). For the OMF records
that TASM produces from this file, see [`OMF.md`](OMF.md).

## File-level framing

| Concern             | Convention                                                |
|---------------------|-----------------------------------------------------------|
| Line endings        | CRLF (`0x0D 0x0A`) on every line, including the last       |
| End-of-file marker  | A single `0x1A` byte (DOS Ctrl-Z) after the final CRLF     |
| Indentation         | Single TAB (`0x09`). No spaces, ever.                      |
| Case                | TASM is case-insensitive by default; BCC emits lowercase   |
|                     | for directives/mnemonics, original case for C symbols.     |
| Filename on disk    | Uppercased basename + `.ASM` (input `hello.c` → `HELLO.ASM`) |
| Filename in content | Lowercased basename + extension (`hello.c`)                |

The `0x1A` EOF byte is a DOS-era courtesy: if `TYPE`-ed at the
console, the terminal stops there. Most modern tools ignore it,
but TASM 2.x expects it for "well-formed" input. BCC always emits
it.

## Lexical conventions

- **Comments**: `;` to end-of-line. BCC uses these heavily for
  source-line tracing (each C statement appears as `   ;\t<text>`).
- **Numeric literals**: bare decimal (`42`). BCC never uses TASM's
  hex `42h` / binary `0101b` forms in operands, even when the
  source has `0x2A`.
- **String literals**: single-quoted bytes for `db` directives
  (`db 'hello',0`). Embedded non-printables get split into separate
  bytes (`db 'he',10,'lo'`).
- **Identifiers**: `[A-Za-z_?@$][A-Za-z0-9_?@$]*`. The `@` and `?`
  prefixes are reserved by Borland for compiler-generated names
  (`@1@50` exit labels, `?debug` records).

## Structural envelope

Every BCC-emitted `.ASM` follows this skeleton, from byte 0 to the
trailing `0x1A`:

```
<macro-preamble>          ; 14 lines, byte-identical across all TUs
<debug-source-record>     ; ?debug S — filename
<debug-comment-record>    ; ?debug C E9... — hex-encoded mtime + name
<segment-scaffold>        ; _TEXT opened-and-closed, DGROUP, _DATA, _BSS
<text-segment-body>       ; per-function emission, in source order
<tail>                    ; re-declared _DATA/_TEXT, public list, end
<0x1A>
```

The macro preamble, segment scaffold, and tail are constant. The
debug records and text-segment body are the only parameterized
sections. ASM_OUTPUT.md walks each in detail.

## Borland-specific extensions

### `?debug` records

A Borland addition to TASM's vocabulary, used for Turbo Debugger
symbol info. Three subforms appear in BCC output:

- `?debug S "<file>"` — declares source filename for the TU.
- `?debug C <hex-bytes>` — generic comment record; bytes are
  forwarded verbatim into an OMF COMENT (see
  [`OMF.md`](OMF.md#per-record-layouts) §COMENT). The leading byte
  of the hex blob is the OMF COMENT class.
- `?debug L <line>` — line-number record. Not yet observed in our
  fixtures; documented for future use.

The macro preamble defines `?debug` as a no-op macro for TASMs that
don't recognize it natively, then conditionally overrides it. This
is why every BCC `.ASM` opens with `ifndef ??version`.

### `publicdll` macro

Borland-specific shorthand for `public` with DLL-export hooks. In
small-model `-S` output it just expands to plain `public name`, but
BCC always defines the macro, even when unused.

### `$comm` macro

Wraps TASM's `comm` (common/external uninitialized symbol) so the
same source works under both old and new TASM ABIs. BCC defines
two variants of the macro guarded by `ifndef ??version`.

### Section-base labels

BCC inserts marker labels at segment opens:

- `d@`, `d@w` at the top of `_DATA` (byte and word views)
- `b@`, `b@w` at the top of `_BSS`
- `s@` at the top of the trailing `_DATA` re-open (string pool)

These appear to be private symbols BCC uses for `bp`-relative or
`ds`-relative addressing computations. TASM treats them as ordinary
labels; the `@` makes them invisible to user code by convention.

### Single-exit labels

Every function has exactly one exit point labeled `@<func-idx>@50`,
where `func-idx` is a 1-based counter over functions in source
order. `50` is the slot number for the epilogue (other slot numbers
appear for switch dispatch, loop tops, etc. — see
[`ASM_OUTPUT.md`](../bcc/ASM_OUTPUT.md#labels-and-slots)).

## Memory model assumptions

Every `.ASM` BCC emits is small-model: one code segment (`_TEXT`),
one combined data segment (`DGROUP = _DATA + _BSS`). The
preamble's `assume cs:_TEXT,ds:DGROUP` pins this. We haven't yet
captured fixtures for medium/large/huge models — if/when we do,
those will get their own section here.

## What TASM does with this

TASM 2.0 reads the `.ASM`, expands macros, resolves symbols, and
emits OMF records into a `.OBJ`. The mapping is mostly mechanical:

| ASM construct                  | OMF record(s)                  |
|--------------------------------|---------------------------------|
| `segment / ends`               | SEGDEF                          |
| `group`                        | GRPDEF                          |
| `public name`                  | PUBDEF                          |
| `extrn name:type`              | EXTDEF                          |
| Code/data bytes in a segment   | LEDATA                          |
| `?debug C <hex>`               | COMENT (class = first hex byte) |
| `?debug S "file"`              | COMENT class 0xe9 (open)        |
| `end` (with optional `start`)  | MODEND                          |

So a `.OBJ` produced by `TASM HELLO.ASM` is structurally what BCC
would produce directly with `-c` — and modulo TASM's own quirks
(record ordering, name-list deduping), the two paths converge on
the same byte stream. The OBJ-emitter slice closed fixture 002 by
short-circuiting the `.ASM → .OBJ` round-trip; see [`OMF.md`](OMF.md)
for what we emit directly.

## Open questions

- Do all BCC memory models share this macro preamble verbatim, or
  do medium/large/huge add segment-override macros? No non-small
  fixtures yet.
- Does `?debug L` ever appear when source-line debug is requested
  via a different `-v` flag combination than what our oracle uses?
- Does TASM 2.0 accept BCC's `.ASM` if the `0x1A` EOF is stripped,
  or does it error out? Worth testing — would tell us whether the
  EOF byte is fingerprint-load-bearing or merely traditional.
