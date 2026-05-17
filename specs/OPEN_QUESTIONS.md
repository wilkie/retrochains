# Open questions for byte-exact BCC compatibility

Things we've observed but can't yet predict from rules. Each entry includes
the smallest input that exposes the gap, what we know empirically, and the
next investigation that would close it. Add new entries as we hit them.

## Publics-list ordering

**The question.** Given a translation unit, in what order does BCC emit the
`public _xxx` directives at the end of the `-S` output (and the corresponding
PUBDEF records in the `-c` OBJ)?

**Why it matters.** PUBDEF order is part of the OBJ byte image. Two units
with the same code but different publics order produce different `.OBJ`
files; we lose byte-exactness even though the linked program would be
identical.

**Current implementation rule (approximate).** Symbols are split by total
mangled-name length (including the leading underscore): length >= 3 goes
to the "long" bucket, length <= 2 goes to the "short" bucket. Each bucket
is emitted reverse-alphabetically, long bucket first. This is documented
in `crates/bcc/src/emit_s.rs::write_tail` and fits the committed fixture
corpus, including fixture 218 (`_main, _g, _f`) where `_f` and `_g` are
both short-bucket symbols.

**Confirmed gap.** Multi-character non-`main` symbols expose violations.
19 captured probes (`int <var> = K; int main(void) {...}` permutations):

| Source | Output |
|--------|--------|
| `aaa, main` | `_main, _aaa` |
| `zzz, main` | `_zzz, _main` |
| `aaaa, main` | `_main, _aaaa` |
| `mmm, main` | `_main, _mmm` |
| `xy, main` | `_xy, _main` |
| `abc, main` | `_abc, _main` |
| `cba, main` | `_cba, _main` |
| `aaz, main` | `_main, _aaz` |
| `abm, main` | `_abm, _main` |
| `abz, main` | `_abz, _main` |
| `p, main` | `_main, _p` |
| `g, main` | `_main, _g` |
| `aaa, mmm, zzz, main` | `_zzz, _main, _mmm, _aaa` |
| `xy, aaa, main` | `_xy, _main, _aaa` |
| `aaa, bbb, main` | `_bbb, _main, _aaa` |
| `int add(); int main` | `_main, _add` |
| `int sum(); int main` | `_sum, _main` |

**What we've ruled out.** No simple closed-form fits all 19 probes:

- byte-sum mod 8 (with or without `_`) — fits 9-11 of 19; `mmm` and `xy`
  resist (sums place them on the wrong side of `main`).
- first-char only, last-char only, first+last mod 8 — fail similar way.
- first-2-bytes interpreted as 16-bit, mod 8 — fails for `mmm`, `xy`.
- length-based, position-weighted sums — fail.
- per-segment reverse-alpha — fails when the unit has any non-`main`
  multi-char variable.
- length-bucket reverse-alpha — fits committed fixtures but does not
  explain the targeted probes listed above.

**Strong hypothesis.** BCC uses a positional/polynomial hash — likely
`h = (h << 4) ^ c` or similar Turbo C-era construction — modulo a specific
table size. The non-monotonicity in byte-sum (`xy` sum 241 hashes higher
than `main` sum 421; `mmm` sum 327 hashes lower) is the giveaway.

**To close.** Either (a) read Turbo C's hashing code if archived Borland
source surfaces, or (b) fit the hash via constraint solving over ~50
targeted probes designed to disambiguate `(h<<K)^c` for various K.

**Slice tag.** Fixture `198-global-ptr-addr-of-elem-obj` exposed this and
was removed; reinstate once the rule is decoded. The AST/parser scaffolding
for `&<ident>[<const>]` is already in the codebase as dormant code.

## LIB archive member provenance and dictionary details

**The question.** BC2.zip's `LIB/*.LIB` files contain the runtime/CRT
archives. Which members are BCC-compiled C objects, which are hand-written
assembler objects, and what archive metadata do we still need to decode?

**Why it matters.** If they're BCC-compiled, the OBJ slice we emit should
already cover their record shapes — a strong end-to-end test. If not,
we'd need separate fixturing for any quirks in their format.

**Status.** Partly answered. `specs/formats/LIB_ARCHIVE.md` records a
probe over CS/MATHS/EMU/FP87/GRAPHICS/OLDSTRMS/CWINS: TLIB strips direct
BCC fingerprint COMENTs, inserts empty class-0xA1 COMENTs, and the
libraries are a mix of BCC-LNAMES-style members and assembler-style
members. CS.LIB and OLDSTRMS.LIB are mostly BCC-style; GRAPHICS.LIB is
entirely assembler-style in that probe.

**To close.** Decode the library dictionary format, run a full COMENT
class histogram over every member, and classify ambiguous members whose
LNAMES/SEGDEF shape is neither canonical BCC nor obvious assembler output.

## Asymmetric `db` style for char-array storage

**The question.** BCC emits two different `db` styles for storage that
contains the same bytes:

- **Named `char[]` global** (`char s[] = "hi"`, fixture 191):
  ```
  _s	label	byte
  	db	104
  	db	105
  	db	0
  ```
  Per-byte numeric form.

- **Anonymous string-pool entry** (`char *p = "hi"`, fixture 192;
  function-scope `"hi"`, fixtures 088/157):
  ```
  s@	label	byte
  	db	'hi'
  	db	0
  ```
  Quoted-string form.

**Why it matters.** Today we emit the right form for each context (per-byte
for named arrays, quoted for the pool). But the *trigger* for picking one
versus the other isn't fully understood — it might be string-pool-vs-named,
or it might be source-syntax-driven (`= "..."` initializer vs. anonymous
literal). The string-pool theory holds across all current fixtures, but
there's no negative case that proves it (e.g. a named array initialized
with a brace-list of bytes — would that take numeric or quoted form?).

**To close.** Capture `char s[] = { 'h', 'i', 0 };` and compare against
`char s[] = "hi"`. If the brace-init form also takes per-byte numeric, the
trigger is "named storage = numeric, pool = quoted". If it takes quoted,
the trigger is something else (probably about the initializer syntax).

## C lexer doesn't accept hex literals

**The question.** Does our C frontend need to support hex (`0xFF`),
octal (`0755`), or other non-decimal integer literal syntax to match the
BC2 corpus?

**Current state.** Our lexer only handles plain decimal integer tokens.
A simple `s.x &= 0xFF;` fails to parse with `bcc: parse: expected ';',
got identifier` at the `0` after the `&=`. Empirically, the oracle BCC
accepts `0xFF` and emits the same bytes it does for `255` — so the gap
is parser-only, not codegen.

**Why it matters.** Fixtures and downstream BC2 sources will use hex
literals for bitmasks, addresses, character codes, and other naturally-
hex values. We need to either (a) extend the lexer to recognize
`0x[0-9a-fA-F]+` (and probably `0[0-7]+` octal), or (b) be willing to
rewrite oracle-corpus inputs to use decimal, which doesn't scale.

**Confirmed gap.** Discovered when capturing fixture 390 (`s.x &= 0xFF;`)
— the oracle bytes for the decimal-rewritten `s.x &= 255;` are identical,
proving the codegen behavior is signedness/value-driven and not syntax-
driven. The lexer is the only blocker.

**To close.** Add hex/octal handling to `lex::lexer`. Capture a few
fixtures using hex literals and check they round-trip. Look for any
oracle-side surprises (e.g. character constants `'\xFF'` for char literals,
or `0L` long suffixes) at the same time.

## Caller side of `>4-byte` struct return needs temp-buffer allocation

**The question.** When the caller assigns the result of a function
returning a >4-byte struct (e.g. `a = f();` for `struct S { int x, y, z; }`),
BCC allocates a temporary stack buffer for the hidden return slot,
passes a far pointer to it as the callee's hidden first arg, then
copies temp → destination via `N_SCOPY@` after the call returns
(verified empirically from the oracle bytes; not yet implemented in
our codegen).

**Why it matters.** The temp-buffer allocation affects the function's
stack frame size (`sub sp, N` in prologue and `mov sp, bp` in
epilogue). For a `main` with no other locals, the temp-buffer is
the only thing that triggers `sub sp` and `mov sp, bp` — without it
the prologue/epilogue is the simpler 3-byte shape. So the buffer
needs to be accounted for at frame-emission time, not just at the
assignment site.

**Confirmed gap.** Fixture 423 (callee body) passes, but fixture 425
was replaced with a discard-result variant because the caller-side
`a = f();` codegen isn't implemented yet. The bytes for the >4-byte
caller side are:
```
sub  sp, 6
mov  ax, OFFSET _a; push ds; push ax     ; eventual dest
push ss; lea ax, [bp-6]; push ax         ; temp buffer (f's hidden arg)
call _f
pop  cx; pop cx                          ; clean up temp ptr only
lea  ax, [bp-6]; push ss; push ax        ; re-push as SCOPY src
mov  cx, 6
call N_SCOPY@                            ; copies temp → _a
```
Note BCC always routes through the temp — even though the
destination is known statically, it doesn't pass `_a`'s far pointer
directly to f as the hidden arg.

**To close.** Add a pre-emission analysis that scans the function
body for `<lvalue> = <call returning >4-byte struct>` and adds the
return type's byte size to the function's stack-frame reserve.
Then emit the buffer-mediated copy at the assignment site. Plus
the corresponding global-dest variant.


