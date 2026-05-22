# `char` / `unsigned char` codegen

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## `unsigned char`

`unsigned char` is a separate type variant (`Type::UChar`) from
`char`. Storage and assignment are byte-identical to `char` —
size 1, alignment 1, same `mov byte ptr DGROUP:_c, K` encoding for
constant assignment (fixture `458`). The two diverge only on:

1. **Comparison.** Unsigned-jump mnemonic family — `jbe/jb/ja/jae`
   instead of `jle/jl/jg/jge`. `Type::is_unsigned()` includes
   `UChar`, so the existing signedness-driven jump-selection path
   picks the right mnemonic automatically. Fixture `459`:
   `if (c > 200)` becomes `cmp byte ptr [_c], 200 / jbe …`.
2. **Int promotion.** Zero-extend via `mov ah, 0` (`B4 00`, 2 bytes)
   instead of sign-extend via `cbw` (`98`, 1 byte). Driven by
   `gty.is_unsigned()` at the load site. Fixture `460`:
   `g = c + 1` becomes `mov al, [_c] / mov ah, 0 / inc ax /
   mov [_g], ax`.

Since storage is shared, almost every `matches!(<t>, Type::Char)`
site in the codegen was a storage-width check (`byte` vs. `word`).
Those were converted en masse to `<t>.is_char_like()` which returns
true for both `Char` and `UChar`. Sign-extension sites stayed
explicit and consult `is_unsigned()` to pick between `cbw` and
`mov ah, 0`.

Parser: the top-level type-probe and `parse_type` both learned the
`unsigned char` sequence. `unsigned` followed by `char` consumes
both tokens and yields `Type::UChar`; the existing post-`unsigned`
`int`-consumption stays intact for the `unsigned int` and bare
`unsigned` forms.

### Coverage in locals, struct fields, arrays

Fixtures `461`–`463` extend uchar to stack-local, struct-field, and
array contexts. The codegen now factors the widening choice into a
single `emit_widen_al(&ty)` helper — applied at every char-load
site (`mov al, byte ptr <addr>` followed by widening). Each site
threads through the leaf type (`leaf_ty`, `pointee`, `field_ty`)
so signed `char` keeps `cbw` and `unsigned char` picks
`mov ah, 0`. The local-assign path also gained a char-aware
immediate-store shape: `mov byte ptr [bp-N], K` (`C6 46 ii ii`, 4
bytes) instead of the previously-unconditional `mov word ptr
[bp-N], K` (`C7 46 ii ww ww`, 5 bytes) for stack-resident char
locals.

### Struct sizing — raw vs. rounded

Fixture `462` (`struct { unsigned char b; int x; }` in `_BSS`)
pinned an unobserved corner: **BCC's struct intrinsic size is the
raw field-sum, with no end-padding to a word boundary**. `_s`
emits 3 bytes of BSS storage, not 4. The earlier "round size up to
even" rule in `parse_record_type` was wrong — it conflated
intrinsic size with local-frame allocation size. Removed the round
for structs (kept for unions until a fixture pins their behavior).

To preserve fixture `102`'s `[bp-4],9` / `[bp-3],42` layout, the
*local frame allocator* now rounds each slot's size up to the
type's alignment before adding it to the running `stack_bytes`
cursor. So a 3-byte struct with alignment 2 occupies a 4-byte
slot, with the struct base at the low (aligned) address and
field offsets layered on top. The previous code already aligned
the *start* of each slot to the type's alignment; rounding the
size up to alignment as well closes the gap.

### `unsigned char` as param / pointee / return

Fixtures `464`–`466` extend uchar to function param, pointer
target, and return type. Two more latent gaps surfaced:

1. **Byte-store through a pointer register** wasn't supported in
   the assembler — `mov byte ptr [si], imm8` (`C6 04 ii`, 3
   bytes). Added as `Instr::MovSiPtrImm8`, paralleling the existing
   word-form `MovSiPtrImm` (`C7 04 ww ww`). Fixture 465 uses it
   for `*p = 200;` where `p: unsigned char *` in SI.
2. **Return value widening differs by signedness**: signed `char`
   return emits `cbw` after the AL load (fixture 156); unsigned
   `char` return emits *no widening* — the value lives in AL alone
   and the upper byte is left undefined. Fixture 466 pins the
   uchar shape: `mov al, byte ptr DGROUP:_g / jmp short epilogue
   / ret`, no `mov ah, 0`. `emit_return_value_load` now
   short-circuits the uchar-ident case to emit just the byte load.

### BSS layout — short bucket first

Fixture `465` (uchar array + int global) exposed that BCC's _BSS
member ordering is **not** pure alphabetical. The actual rule:
*short-named globals (mangled `_<name>` length < 3) in alphabetical
order first, then long-named globals in alphabetical order*. This
is the same length-bucket discriminant the publics emission uses,
applied to globals (and effectively the reverse of the publics
emission order, filtered to BSS members).

The earlier "alphabetical" reading happened to coincide on
fixtures 181/234/462 because their names all fell in the same
bucket. 465 has `_buf` (4 chars including underscore) in the long
bucket and `_g` (2 chars) in the short bucket, so the buckets
diverge: oracle emits `_g, _buf` (short → long) with no padding,
not the alphabetical `_buf, _g` that would require a 1-byte pad
to align `_g`.

### Publics emission — long bucket direction

The long bucket's sort direction depends on whether the source
has either a short-named global *or* a long-named **initialized**
global (one that lands in `_DATA`). Fixture `498`
(`char msg[16] = "hello"; int main { return 0; }`) forced the
latest refinement: even with no short global, a `_DATA`-resident
long global flips the order to forward. The current rule:

- If the long bucket contains a global **and** the source has at
  least one short-named global, **or** the source has at least one
  long-named global with an initializer (in `_DATA`): emit the
  long bucket in **forward** alphabetical order (globals,
  functions, helpers mixed together).
- Otherwise: emit in **reverse** alphabetical order.

Pinning fixtures:

- 095 (`_sum`, `_main`) — no short global, no DATA global →
  reverse → `_sum, _main`.
- 179 (`_add`, `_main`) — same → reverse → `_main, _add`.
- 260 (`_main`, `N_LXMUL@`) — short globals `_a, _b` present, but
  long bucket has no global → reverse → `_main, N_LXMUL@`.
- 465 (`_buf`, `_main`) — long global + short global `_g` →
  forward → `_buf, _main`.
- 491 (`_pts`, `_main`) — long global + short global `_g` →
  forward → `_main, _pts`.
- 494 (`_head`, `_main`) — long BSS global only, no short global,
  no DATA global → reverse → `_main, _head`.
- 498 (`_msg`, `_main`) — long DATA global (`msg[16] = "hello"`)
  → forward → `_main, _msg`.

Fixture `506` (`int helper(int); int main { return helper(7); }
int helper(int x) { ... }`) adds a third trigger: a function
prototype (`body: None` followed later by a matching definition)
flips the order to forward. Updated rule:

- If `(long has global AND short has global)` OR `long has an
  initialized DATA global` OR `there is any function prototype`,
  use **forward** alphabetical for the long bucket.
- Otherwise use **reverse**.

Pinning fixture: 506 → forward → `_helper, _main`.

The intuition is that BCC's internal iteration order is sensitive
to which symbol table buckets are non-empty: initialized-data
globals, short-named globals, and seen-twice symbols (prototype +
def) all leave a record that flips a "forward iteration" mode for
the long bucket. The exact mechanism remains unclear; further
fixtures may force more refinement.

## Char local compared to constant

Fixture `524` (`char c; c = 'A'; if (c == 'B') ...`) — the
stack-local compare path in `emit_compare` always emitted `cmp
word ptr [bp-N], K`, but for a char local BCC uses the byte
form `cmp byte ptr [bp-N], K` (encoded `80 7E disp8 imm8`). The
fix: check `ty.is_char_like()` on the named local and emit the
byte form. A new IR variant `CmpByteBpRelImm8` encodes it.
Parser handles `cmp byte ptr [bp+N], imm8` via the existing
`parse_byte_bp_relative` helper.

## `-d` merges duplicate string literals (single `_DATA` copy); `-K` makes `char` unsigned (zero-ext via `mov ah, 0`)

Fixtures `2282` (-d merge effect), `2283` (no -d
baseline), `2284` (-K unsigned char) characterise
data-side flags.

- `2282` (**-d merge strings**): both pointers
  `a` and `b` point to the same string in
  `_DATA`:
  ```
  _DATA layout (-d):
    [04 00]              ; a → offset 4
    [04 00]              ; b → offset 4 (SAME!)
    [hello\0]            ; "hello" at offset 4
  
  Total: 10 bytes
  ```
  `a == b` returns 1.
- `2283` (**no -d, separate strings**):
  ```
  _DATA layout (no -d):
    [04 00]              ; a → offset 4
    [0a 00]              ; b → offset 10 (different)
    [hello\0]            ; first copy at offset 4
    [hello\0]            ; second copy at offset 10
  
  Total: 16 bytes
  ```
  `a == b` returns 0. Code generated is identical
  between -d and no-d; only data differs.
- `2284` (**-K unsigned char**): `(int)char` uses
  zero-extend `mov ah, 0` instead of `cbw`:
  ```
  ; With -K, c = 200 (unsigned char):
  mov al, [c]
  mov ah, 0                ; b4 00 (zero-extend)
  mov [n], ax              ; n = 200
  ```
  Without -K (signed char), `cbw` would sign-
  extend 0xC8 → 0xFFC8 = -56.

**Data-side flag effects**:
| Flag | Effect on data | Effect on code |
|------|----------------|----------------|
| `-d` | Merge duplicate string literals | None |
| `-K` | None | Char→int uses zero-ext (mov ah, 0) |
| `-f-` | None (no float linkage) | (probable) |

**Char signedness comparison**:
```
char c = 200;     // 0xC8 — signed = -56, unsigned = 200
int n = c;

// Without -K (signed char):
mov al, [c]
cbw            ; ah = 0xFF (sign bit replication) → n = 0xFFC8 = -56

// With -K (unsigned char):
mov al, [c]
mov ah, 0       ; ah = 0x00 → n = 0x00C8 = 200
```

**String literal merging**:
- Without `-d`: each occurrence of `"hello"` gets
  its own slot in `_DATA`
- With `-d`: identical strings share a single
  slot, reducing data size
- Comparison: `"abc" == "abc"` (literal == literal)
  is TRUE with -d, FALSE (usually) without

For the Rust reimplementation:
- `-d`: implement string-literal deduplication
  pass before `_DATA` emission.
- `-K`: make char default unsigned; emit
  `mov ah, 0` for char→int casts.

## Char literal = parse-time int; `\xNN`/`\NNN` byte escapes; adjacent strings concat at parse

Fixtures `2111` (char literals), `2112` (hex/
octal escapes in strings), `2113` (adjacent
string concat) cover three more lexer behaviours.

- `2111` (**char literal = parse-time int**):
  `'A'`, `'\n'`, `'\0'` evaluate to 65, 10, 0
  respectively at parse time. Each fits in
  `imm16`, stored via `c7 46 disp imm16`. Per
  C standard, char literals have type `int`.
- `2112` (**hex/octal byte escapes**): `"\xAB
  \101\x7F"` decodes to `ab 41 7f`:
  - `\xNN` = hex byte (1-2 hex digits)
  - `\NNN` = octal byte (1-3 octal digits, `\101
    = 65 = 'A'`)
  
  Both are byte-valued, not int — they fit in a
  single char.
- `2113` (**adjacent strings concatenate at
  parse**): `"Hello, " "World!"` emitted as the
  single string `Hello, World!` (13 chars + null
  = 14 bytes in `_DATA`). Concatenation is a
  lexical/preprocessing step per C standard. NO
  runtime concat — single literal.

**Lexer summary (lex-time data)**:
| Token | Result | Type |
|-------|--------|------|
| `0xABCD` | hex int | `int` (or `unsigned int` if > INT_MAX) |
| `0177` | octal int | `int` (= 127) |
| `100` | decimal int | `int` |
| `'A'` | int (65) | `int` |
| `'\n'` | int (10) | `int` |
| `'\xAB'` | int (171) | `int` |
| `"abc"` | char[4] | `char[]` (with implicit `\0`) |
| `"a" "b"` | concatenated string | single `char[]` |
| `'\101'` (in string) | octal byte (65) | byte in string |

For the Rust reimplementation:
- Lex char literal → int value (parse the escape).
- Lex string literal: parse all escapes, concat
  adjacent strings, append implicit `\0`.

## Partial arr init zero-pads tail; implicit size = init count; `char s[]="abc"` includes `\0`

Fixtures `2093` (partial int init), `2094`
(implicit size), `2095` (char arr from string)
cover array initializer semantics.

- `2093` (**`int arr[5] = {1, 2}`**): explicit
  values fill from index 0; remaining slots
  **zero-padded**. Data emits `01 00 02 00 00
  00 00 00 00 00` (10 bytes total — matches
  declared size, NOT init count).
- `2094` (**`int arr[] = {10, 20, 30, 40}`**):
  **implicit size = init count**. Data emits
  `0a 00 14 00 1e 00 28 00` (8 bytes for 4
  ints). Array sized to fit the explicit list.
- `2095` (**`char s[] = "abc"`**): sized to
  `strlen + 1` to include the **null terminator**.
  Data emits `61 62 63 00` (4 bytes for 3-char
  string).

**Array initializer summary**:
| Form | Sizing | Tail filling |
|------|--------|---------------|
| `int arr[5] = {1, 2}` | declared (5) | zero-pad remaining |
| `int arr[5] = {1, 2, 3, 4, 5}` | declared (5) | none (full) |
| `int arr[] = {1, 2, 3}` | init count (3) | n/a |
| `char s[] = "abc"` | strlen+1 (4) | n/a |
| `char s[3] = "abc"` | declared (3) | might TRUNCATE null! |
| `char s[10] = "abc"` | declared (10) | zero-pad tail |

For string literals as array initializers, the
**null terminator is implicitly included** when
size is implicit or sufficient.

For the Rust reimplementation:
- Track declared vs implicit array size.
- Emit explicit init values in order.
- For declared > init count: emit zero bytes for
  the remainder.
- String literal init: include `\0` if room
  remains.

## Memory-model byte-level diffs: near vs far code (`c3` vs `cb` ret); code-seg name per-file in mm/ml

**First pivot away from small-only**: fixtures
`2051` (compact -mc), `2052` (medium -mm), `2053`
(large -ml) all compile **the same source**
`int g = 42; int main() { return g; }`. The
differences are the **memory model**.

- `2051` (**compact, -mc**): **identical to small**
  for this trivial source. Single `_TEXT` segment,
  `5d c3` (pop bp / near ret). Data in `_DATA` via
  DGROUP, access by `a1 disp16`.
- `2052` (**medium, -mm**): code segment renamed
  to **`HELLO_TEXT`** (= per-file `<fname>_TEXT`).
  Return is **`5d cb`** (pop bp / **far ret**).
  Data still `a1 disp16` via DGROUP (near data).
- `2053` (**large, -ml**): byte-identical to
  medium for this trivial test — `HELLO_TEXT` for
  code, `5d cb` for far ret, `a1 disp16` for
  data.

**Memory-model summary** (from these probes):
| Model | Code | Data (trivial) | Code seg | RET |
|-------|------|----------------|----------|-----|
| small (-ms) | near | near | `_TEXT` | `c3` |
| compact (-mc) | near | near | `_TEXT` | `c3` |
| medium (-mm) | far | near | `<fname>_TEXT` | `cb` |
| large (-ml) | far | near | `<fname>_TEXT` | `cb` |
| huge (-mh) | far | far | (not yet probed) | (not yet probed) |

So the two key bits visible in OBJ output:
1. **Code segment naming**: small/compact use the
   single global `_TEXT`; medium/large use a
   per-source-file `<fname>_TEXT`.
2. **Function return**: small/compact use `c3`
   (near ret 0xC3); medium/large use `cb` (far
   ret 0xCB).

Data access in the trivial case is identical (`a1
disp16`) for all four because the global g lives
in DGROUP'd `_DATA`. To distinguish compact/large
from small/medium, we'd need multi-segment data or
explicit `far` data — not yet probed.

For the Rust reimplementation:
- Parse model from `-ms` / `-mc` / `-mm` / `-ml`.
- Code segment: emit `_TEXT` for s/c, `<fname>_TEXT` for m/l.
- Function epilogue: emit `c3` for s/c near ret, `cb` for m/l far ret.
- Data: keep DGROUP near in all base cases.

## Block-slot reuse: N sequential blocks share; nested respects scope; byte-granular sharing

Fixtures `1967` (3 sequential blocks), `1968`
(nested blocks), `1969` (different-sized block
locals) refine the block-slot-reuse rule.

- `1967` (**N sequential blocks share slot**):
  three sequential blocks each with a local
  variable all share **the same `[bp-2]` slot**:
  ```
  ; block 1: a
  c7 46 fe 01 00   ; a = 1 at [bp-2]
  03 76 fe          ; sum += a
  ; block 2: b — reuses [bp-2]
  c7 46 fe 02 00   ; b = 2
  03 76 fe          ; sum += b
  ; block 3: c — reuses [bp-2] again
  c7 46 fe 03 00   ; c = 3
  03 76 fe          ; sum += c
  ```
  Only 1 slot allocated for N sequential
  non-overlapping locals.
- `1968` (**nested blocks respect outer scope**):
  ```c
  int outer = 100;
  { int inner = 50; outer += inner; }
  ```
  Outer enregisters (used twice — init + return)
  in SI. Inner gets `[bp-2]`. **No slot reuse
  between outer and inner** since their lifetimes
  overlap (outer is still alive when inner is
  created).
- `1969` (**byte-granular slot sharing**):
  different-sized block locals can share too:
  ```c
  { long a = 100L; sum += (int)a; }    // a = 4 bytes at [bp-4..bp-1]
  { int b = 50;    sum += b; }         // b = 2 bytes at [bp-2..bp-1]
  ```
  `b` reuses **the HIGH HALF of a's slot** (the
  top 2 bytes). So slot reuse is **byte-
  granular** — BCC tracks the actual byte ranges
  of dead variables, not just whole-variable
  slots.

So the block-slot-reuse algorithm is:
1. Track byte-range liveness for every local
2. When entering a block, look for **N
   contiguous dead bytes** (from finished
   scopes) before allocating new stack
3. Allocate at the lowest available range that
   fits

For the Rust reimplementation:
- Maintain a per-byte-range liveness map within
  the function's stack frame.
- On entering a block, scan for free byte-ranges
  of the required size; reuse if found.
- Else allocate new bytes at the bottom of the
  current frame (incrementing `sub sp` by the
  new variable's size).
- On block exit, mark the variable's byte range
  as dead/free.

This single optimization keeps stack frames as
compact as possible — frame size = max
"concurrently live local bytes" across the
function.

## Tiny = same as small (model byte only); Huge adds DS save/restore + `N_PADA@`

Fixtures `1769` (`-mt` tiny), `1770` (`-mh` huge),
and `1771` (huge ptr arith in -ms) complete the
**full 6-model coverage**.

- `1769` (**`-mt` tiny**): produces **byte-
  identical code to `-ms`** at the per-function
  level. Same `_TEXT`/`_DATA` segments, same call/
  ret encoding. Differs only in the **COMENT model
  marker byte** (`08` for tiny vs `09` for small)
  which tells the linker to merge all segments
  into one (max 64K total program).
- `1770` (**`-mh` huge**): introduces **per-module
  DS swap** in every function:
  ```
  _func:
  push bp
  mov bp, sp
  push ds                   ; 1e — save caller's DS
  mov ax, MOD_DGROUP        ; b8 imm16 (FIXUPP)
  mov ds, ax                ; 8e d8 — switch to our DGROUP
  ; body
  pop ds                    ; 1f — restore caller's DS
  retf                      ; cb
  ```
  Plus new segment name `HELLO_DATA` (the module's
  own data segment, parallel to `HELLO_TEXT` for
  code). So huge model adds **~6 bytes of DS save/
  load** to every function prologue and **1 byte
  of DS restore** to every epilogue. Each TU
  effectively has its own data segment.
- `1771` (**huge ptr arith uses `N_PADA@`**):
  with `int huge *p`, doing `p++` calls **`N_PADA@`**
  (huge pointer add). The helper:
  - **DX:AX** = far ptr to operand pointer
  - **CX:BX** = 32-bit increment value
  - Adjusts the segment:offset so the result is
    properly **normalized** across 64K boundaries

  This is the **key distinction** between far and
  huge pointers:
  - `far *` `p++`: increments offset only (1651) —
    breaks at 64K boundaries
  - `huge *` `p++`: uses `N_PADA@` to renormalize
    the segment:offset — properly handles >64K data

Full 6-model coverage matrix:
| Model | Code | Data | Seg name | retX | call site | Special |
|-------|------|------|----------|------|-----------|---------|
| `-mt` tiny | near | near | `_TEXT` | ret | call near | merged at link |
| `-ms` small | near | near | `_TEXT` | ret | call near | (default) |
| `-mc` compact | near | far  | `_TEXT` | ret | call near | & = 4B fp |
| `-mm` medium | far  | near | `HELLO_TEXT` | retf | push cs/call | |
| `-ml` large | far  | far  | `HELLO_TEXT` | retf | push cs/call | & = 4B fp |
| `-mh` huge | far  | far  | `HELLO_TEXT`+`HELLO_DATA` | retf | push cs/call | DS swap, N_PADA@ |

And **huge-pointer helper**:
| Helper | Purpose | ABI |
|--------|---------|-----|
| `N_PADA@` | huge ptr addition | dx:ax = ptr, cx:bx = inc; normalises |
| (presumably `N_PCMP@`, `N_PSBA@`, etc. for cmp/sub) | not yet probed | |

This completes the cross-model story. For the Rust
reimplementation, all 6 models are characterised
and the model-conditional emission is well-bounded.

## `int→char` = byte load; `uchar→int` = `mov ah,0`; `schar→int` = `cbw`

Fixtures `1730` (int→char cast), `1731` (uchar→int
zero-extend), and `1732` (signed char→int sign-
extend) characterise the char/int conversion
codegen.

- `1730` (**`(char)int` narrowing**): no explicit
  shift or mask — just **`mov al, byte [m]`** to
  read the low byte. The 1-byte store `mov [c], al`
  (`88 /N`) then commits. Truncation is implicit
  in the byte-width access. So narrowing is **free**:
  ```
  mov al, byte [int_var]    ; 8a /N (read low byte)
  mov [char_var], al         ; 88 /N (store byte)
  ```
- `1731` (**`(int)unsigned char` zero-extend**):
  uses **`mov ah, 0`** (`b4 00`, 2 bytes) to zero
  the high byte after `mov al, [uc]`. The
  combination yields AX with low byte = uc and high
  byte = 0.
- `1732` (**`(int)signed char` sign-extend**): uses
  **`cbw`** (`98`, 1 byte) — convert byte to word
  by sign-extending AL into AH. Mirrors the int→
  long pattern (`cwd`):
  - `cbw` (`98`) — AL → AX (sign extend)
  - `cwd` (`99`) — AX → DX:AX (sign extend)

So the widening conversions follow the same per-
signedness rule across all widths:
| Conversion | Signed | Unsigned |
|------------|--------|----------|
| `char → int` | `cbw` (1B) | `mov ah, 0` (2B) |
| `int → long` | `cwd` (1B) | `mov word [high], 0` (5B) |

**Char-init byte instructions**:
- `mov byte [m], imm8` = `c6 /N + disp + imm8`
- `mov al, byte [m]` = `8a /N`
- `mov byte [m], al` = `88 /N`

So initing a char with a constant uses `c6 imm8`
(3-4 bytes), distinct from `c7 imm16` for words.

This completes the char/int conversion picture.
Char arithmetic always happens at int width (via
implicit promotion); narrowing back to char drops
the high byte without explicit work.

## addr-taken → forced to memory, `(uchar)int` vs `(char)int` cast widening

Fixtures `1532` (ternary picks between `&x` and
`&y`, then stores through the resulting pointer),
`1533` (`int x; return (unsigned char)x;` zero-
extend cast on int), and `1534` (`int x;
return (char)x;` signed cast on int) all pass on the
first capture.

- `1532` (**refinement**): `x` and `y` both have
  their addresses taken (`&x`, `&y`). They stay on
  the stack (`[bp-2]`, `[bp-4]`) regardless of their
  use count — the address would have nowhere to
  point if they lived in a register. So
  **address-taken locals are forced to memory** as
  an *additional* constraint on top of the
  use-count rule. The ternary itself lowers
  straightforwardly: cmp / inverse-jcc / `lea ax,
  [&x]` arm / jmp / `lea ax, [&y]` arm. Pointer `p`
  goes to SI; `*p = 99` is `mov [si], 99`.
- `1533`: `(unsigned char)x` on an int lowers to
  **`mov al, [bp-2] / mov ah, 0`** — byte-load then
  zero-extend. Same widening idiom as the char →
  unsigned char cast in [[batch-402-comma-cast-shr]]
  fixture `1524`. The cast is implemented as
  "ignore high byte of source, then zero-extend in
  destination" without an explicit AND.
- `1534`: `(char)x` on an int lowers to **`mov al,
  [bp-2] / cbw`** — byte-load then signed sign-
  extend. The 1-byte `cbw` (opcode `0x98`) saves a
  byte versus the `mov ah, 0` (`b4 00`, 2 bytes) of
  the unsigned variant. So the encoder produces:
  | Cast | Sequence | Bytes |
  |------|----------|-------|
  | `(unsigned char)int` | `mov al,[m] / mov ah,0` | 5 |
  | `(char)int`          | `mov al,[m] / cbw`      | 4 |

So the encoder treats narrowing-then-implicitly-
widening as a byte-load of the low part followed by
the appropriate sign/zero extension. Signed casts
get the shorter encoding for free via `cbw`.

## `pow(2,5)`, globals `a*b - 5`, signed `(-20)/4` char

Fixtures `1355` (`int pow(int b, int e) { int r=1; ...
for (i=0;i<e;i++) r *= b; return r; }` — integer power
function via loop), `1356` (`int a=10; int b=3; return
a * b - 5;` — arithmetic on two file-scope int
globals), and `1357` (`char a=-20; char b=4; return a/
b;` — signed char division yielding negative result)
all pass on the first capture. `1355` confirms a
non-recursive power loop: `r=1, e=5, b=2` iterates
five `r *= 2` mults, returning 32. The for-loop step
combined with `r *= b` exercises both compound-mul-var
and loop-step lowering. `1356` confirms global-global
arithmetic at file scope: `mov ax,[_a] / imul word
ptr [_b] / sub ax,5`. Result 10*3-5 = 25. `1357`
confirms signed char division: both chars `cbw`-
extended to int, then `cwd / idiv` for signed
division. -20/4 = -5, returned as int. Exit code = 256
-5 = 251.

## `char * char`, fn modifies param, `char *` arg write-through

Fixtures `1223` (`char a=5; char b=4; int c = a*b;
return c;` — char times char with int destination),
`1224` (`int f(int x) { x++; return x; } return f(10);`
— callee mutates its param), and `1225` (`void f(char
*p){ *p = 7; } char c=0; f(&c); return c;` — caller
passes `&c`, callee writes through the pointer arg) all
pass on the first capture. `1223` confirms char×char
goes through the standard char-to-int promotion: both
operands `cbw`'d into AX/DX, `imul dx`, store the
16-bit result into the `int` slot — no narrow-form
mul. `1224` confirms params live in the same
slot-relative frame as locals: `x++` lowers to `inc
word ptr [bp+offset]` where `[bp+offset]` is the
positional arg slot above the saved BP, and the return
re-reads from that same slot. `1225` confirms the
`void`-returning fn shape (no AX setup at return) plus
`*p = const` byte store: callee `mov bx,[bp+arg] / mov
byte ptr [bx],7`, caller stores `&c` to the slot and
reads it back after the call.

## char OR / XOR const, char `!` as value

Fixtures `956` (`char c = 15; return c | 4;` — char bitwise
OR with int constant, returned as value), `957` (`return c
^ 4;` — char XOR const), `958` (`char c = 0; return !c;` —
logical NOT of a char as a return value).

All three already work end-to-end:

- 956 / 957: the existing char-with-int-const arithmetic
  path widens the char via `cbw` (or `mov al, byte ptr
  [bp-1]; cbw`) and then emits `or ax, 4` / `xor ax, 4`
  against an int-typed AX. The 16-bit OR/XOR with a small
  constant uses the imm8sx Group-1 encoding (`83 /1 dd ii`
  for OR, `83 /6 dd ii` for XOR), one byte shorter than the
  imm16 form. Sibling of fixture 609 (char AND const).
- 958: `!c` for a char operand lowers exactly like `!int` —
  the operand is widened to AX, then the boolean-NOT
  materialization runs through the standard mini-CFG
  (`or ax, ax; jz .true; xor ax, ax; jmp .end; .true: mov
  ax, 1; .end:`). The widening is `mov al, byte ptr [bp-1];
  cbw`, and the rest is the same as fixture 618's `!x`
  path on int.

## int `--x`, `x--` as value, `char == -1`

Fixtures `941` (`x = 5; y = --x;` — int pre-decrement as
value), `942` (`x = 5; r = x--;` — int post-decrement as
value), `943` (`char c = -1; if (c == -1) return 1;` — char
compared against a negative literal).

All three already work end-to-end:

- 941: pre-decrement-as-value lowers to the same in-place
  decrement followed by an AX load, mirroring the
  pre-increment shape (fixture 530). BCC's pattern is `dec
  word ptr [bp-N]; mov ax, [bp-N]` — the decrement modifies
  the variable, and the AX load reads the *new* value as the
  expression's result.
- 942: post-decrement-as-value is the dual: `mov ax, [bp-N];
  dec word ptr [bp-N]` — AX captures the *old* value before
  the in-place decrement modifies the variable.
- 943: `c == -1` with `c` a signed char promotes the char to
  int via `cbw`, then compares with the int constant `-1`
  (`0xFFFF`). BCC's pattern: `mov al, byte ptr [bp-1]; cbw;
  cmp ax, -1`. The promotion is what makes the comparison work
  — without sign-extension, `0xFF == 0xFFFF` would fail.

## Char init, void function, const variable

Fixtures `926` (`char c = 'A';` — char global with char-literal
init), `927` (`void set5(void) { g = 5; }` — void function
with explicit void params), `928` (`const int c = 5; return c;`
— const-qualified int global).

All three already work end-to-end. Coverage:

- 926: char literal `'A'` lowers to integer `65` masked to 8
  bits; `db 65` lands in `_DATA` under `_c`.
- 927: void return type plus explicit `void` parameter list —
  function body just sets globals, return path emits bare
  `ret` (no value materialization).
- 928: `const` qualifier on a global accepted at parser; codegen
  treats the global the same as a plain int. The qualifier
  doesn't change the OBJ — no read-only segment, no extra
  attributes. Modification through a writable lvalue would be
  UB but the codegen doesn't enforce it.

**Recorded findings (deferred):**

- **Array of pointers with non-const RHS** (`int *q[2]; q[0] =
  &a`): codegen panics "non-constant rhs in constant-indexed
  global array assign not yet supported" — the global-array
  store path's RHS handling doesn't yet accept address-of
  expressions for pointer-typed elements. Needs an arm in
  `emit_array_assign`'s global-array branch.
- **Main with command-line args** (`int main(int argc, char
  *argv[])`): same parser failure as fixture 922's array-as-
  parameter — `T name[]` in the parameter list panics "expected
  `)`, got `[`". Sized arrays (`char argv[16]`) likely also
  fail; the workaround in callers' code is `char **argv`.

## `char` update as function argument (stack-resident)

Fixtures `731` (`f(c++)`), `732` (`f(++c)`), `733` (`f(c--)`)
— BCC chose to keep `c` stack-resident here (the function-
arg expression context apparently affects the allocator's
eligibility check; not yet pinned to a rule). The generic
`emit_update_to_ax` previously panicked with "stack-
resident local not yet supported" for any char update.

Added a stack-resident char branch to `emit_update_to_ax`
with the same pre/post asymmetry observed elsewhere:

- **Post** (`c++`): `mov al, byte ptr [bp-N]; inc byte ptr
  [bp-N]; cbw` — captured value first, then memory-direct
  side effect, then widen.
- **Pre** (`++c`): `mov al, byte ptr [bp-N]; inc al; mov
  byte ptr [bp-N], al; cbw` — AL detour. Stack-char pre
  takes the same shape as char-global pre (batch 128) and
  char-field pre (batch 130): BCC threads the new value
  through AL rather than memory-direct `inc byte ptr`.

Unsigned uses `mov ah, 0` for the widening step.

## `char` update as expression result (int destination)

Fixtures `728` (`int r = c++`), `729` (`int r = ++c`),
`730` (`int r = c--`) — char update result widened into an
int destination.

The generic `emit_update_to_ax` path produces the right
*instructions* (load, widen, store, side-effect) but in the
wrong *order* — BCC stores before the side effect on Post,
and threads through AL with explicit write-back on Pre. Added
two more char-aware fast paths in `emit_assign_local`:

- **Char→int Post**: `mov al, <src>; cbw; mov [bp-N], ax;
  inc <src>` — store the widened value, then bump source.
  Unsigned uses `mov ah, 0` for widening.
- **Char→int Pre**: `mov al, <src>; inc al; mov <src>, al;
  cbw; mov [bp-N], ax` — bump AL, write back to source,
  widen, store.

Together with batch 136 (char destination), the byte-source
update-as-expression path now matches BCC for both
destination widths and both pre/post positions.

## `char` update as expression result (char destination)

Fixtures `725` (`d = c++`), `726` (`d = c--`),
`727` (`d = ++c`) — char update result captured into another
char (the batch-135 deferred case).

The generic `emit_update_to_ax` path widens through AL with
`cbw`, which is wasted work when the destination is byte —
BCC keeps everything in AL without the widen. Added two
char-aware fast paths in `emit_assign_local`'s
`LocalLocation::Stack(off)` arm (parallel to the existing
int fast path at line 7253):

- **Post**: `mov al, <src>; mov byte ptr [bp-N], al; inc
  <src>` (store the captured value, then bump source).
- **Pre**: `mov al, <src>; inc al; mov <src>, al; mov byte
  ptr [bp-N], al` (bump AL, write back to BOTH source and
  destination from AL). Note BCC threads the new value
  through AL rather than incrementing the source register
  directly — keeps everything single-source so AL holds the
  expression result for the subsequent store.

The asymmetry vs the int destination path is that BCC also
threads the int through AX with a single store; we don't
yet need a different shape for that since `inc dl` followed
by `mov [bp-N], dl` would skip the AL write. (Confirmed: the
int fast path stores the register directly.)

## `char` global pre vs post: `--g`, `g++`, `g--`

Fixtures `701` (`--g`), `702` (`g++`), `703` (`g--`).

- `701` and `703` — free passes off batch 127's char-global
  update path (pre uses AL detour, post uses memory-direct).
- `702` exposed that **BCC differentiates pre vs post even
  when the result is discarded**. For `++g;` as an
  expression statement, BCC emits the AL load-modify-store
  (`mov al, _g; inc al; mov _g, al`, fixture 700); for
  `g++;` discarded, BCC emits memory-direct
  `inc byte ptr _g` (fixture 702). The two compile to
  *different* machine code despite producing the same side
  effect on `g`. Apparently BCC's pre-update lowering always
  materializes the new value in AL even when caller doesn't
  use it.
- Threaded `UpdatePosition` through `emit_update_in_place`
  and branched the char-global case on it. Pre → AL detour;
  post → memory-direct. Added `IncGroupSymByte` /
  `DecGroupSymByte` tasm IR variants (`FE 06|0E lo hi` +
  FIXUPP, Grp4 r/m8 with mod=00 r/m=110) and the matching
  parser entries (`inc byte ptr ...` / `dec byte ptr ...`).


## `char s[] = "hi"` auto-storage — uses N_SCOPY@ runtime copy

Fixture `2509-char-arr-from-strlit-obj`:

```c
int main(void) {
  char s[] = "hi";
  return s[0];
}
```

```
55 8b ec                  prologue
83 ec 04                  sub sp, 4              ; 4B frame (3B padded to even)
16                        push ss
8d 46 fc                  lea ax, [bp-4]          ; dst
50                        push ax
1e                        push ds
b8 00 00                  mov ax, 0               ; src offset (FIXUPP _DATA)
50                        push ax
b9 03 00                  mov cx, 3               ; count incl NUL
e8 00 00                  call N_SCOPY@           ; (EXTDEF)
8a 46 fc                  mov al, [bp-4]
98                        cbw
eb 00 8b e5 5d c3         epilogue
```

Findings:
- An auto-storage `char[]` initialized from a string literal is
  NOT inlined as a sequence of byte stores — BCC emits a runtime
  **`N_SCOPY@`** call with the four-arg signature
  `(ss, dst-off, ds, src-off, count)`. The source `"hi\0"` lives
  in `_DATA`.
- **Stack-allocated char arrays are even-padded**: source is 3
  bytes ("h", "i", "\0") but the local reserve is `sub sp, 4`.
  Stack alignment is byte-pair (even) at minimum.
- The string literal itself lands in `_DATA` (not `_TEXT` /
  read-only segment) — confirmed by the FIXUPP target being the
  `_DATA` symbol with disp 0.
- This is the same `N_SCOPY@` helper used for struct-by-value
  returns and for some larger struct copies. So small `char[]` and
  large structs share the helper path.


## Char-returning function — loads AL only, leaves AH untouched

Fixture `2514-noarg-char-ret-obj`:

```c
char which(void) {
  return 'Q';
}
```

```
55                            push bp
8b ec                         mov bp, sp
b0 51                         mov al, 0x51  ; 'Q' — AL only, AH UNTOUCHED
eb 00                         jmp $+2       ; default-position epi
5d                            pop bp
c3                            ret
```

Findings:
- Char-return loads ONLY `al` with the character literal (opcode `b0`,
  2 bytes). The high byte `ah` is never written — callers reading
  the return as a `char` use only AL, so AH is undefined.
- Saves 1 byte vs the int-return `mov ax, imm16` form (`b8 51 00`).
- The default-position `eb 00` jump-to-epi IS emitted. **Confirms**:
  non-void return functions ALWAYS emit `eb 00` before their pop bp,
  regardless of body length (compared to void's body-falls-through-
  into-pop pattern).
- This is the canonical char-return prologue/body/epilogue shape;
  it's the shortest possible function except for the absolute minimum
  (a void function with empty body would be `55 8b ec 5d c3` = 5
  bytes; this is 9).


## `char` parameter — caller pushes 16-bit slot, callee reads AL + `cbw`

Fixture `2523-char-param-promote-obj`:

```c
int compute(char c) {
  return c + 1;
}
```

```
55 8b ec                    prologue
8a 46 04                    mov al, [bp+4]          ; LOW BYTE only
98                          cbw                     ; sign-extend al → ax
40                          inc ax                  ; c + 1 peephole
eb 00 5d c3                 epilogue
```

Findings:
- The `char` parameter occupies a **full 16-bit stack slot**
  (cdecl pushes all args as words/larger). The callee reads only
  the **low byte** via `mov al, [bp+4]` (opcode `8a`, byte form),
  leaving AH stale.
- BCC then emits `cbw` (1 byte) to **sign-extend AL into AX**
  before any int-context arithmetic. This is the integer promotion
  for plain `char` (signed). Compare to `unsigned char` which would
  use `xor ah, ah` (2 bytes) to zero-extend.
- `c + 1` collapses to `inc ax` (1 byte) — same peephole as int+1.
- So char-param promotion shape is **`8a 46 disp; 98; ...`** (=4
  bytes total to get a sign-extended int into AX). A reusable
  template.


## `unsigned char` parameter — uses `mov ah, 0` (NOT `xor ah, ah`)

Fixture `2525-uchar-param-promote-obj`:

```c
int compute(unsigned char c) {
  return c + 1;
}
```

```
55 8b ec                    prologue
8a 46 04                    mov al, [bp+4]     ; low byte
b4 00                       mov ah, 0          ; ZERO-EXTEND
40                          inc ax             ; c + 1
eb 00 5d c3                 epilogue
```

Findings:
- `unsigned char` promote uses **`mov ah, 0`** (`b4 00`, 2 bytes) to
  zero-extend — NOT `xor ah, ah` (also 2 bytes, but clobbers flags).
  Both are the same length; BCC prefers the flag-preserving form.
- Contrast with **signed `char`** (`2523`) which uses `cbw` (1 byte)
  to sign-extend AL → AX.
- Promote-template byte signatures:
  - Signed char:   `8a 46 disp; 98`         (4B)
  - Unsigned char: `8a 46 disp; b4 00`      (4B)
  Same total length; differ only in the third+ bytes.


## `unsigned char` return — `and al, imm8` (AL-accumulator form)

Fixture `2539-uchar-ret-obj`:

```c
unsigned char low(int x) {
  return (unsigned char)(x & 0xFF);
}
```

```
55 8b ec                       prologue
8a 46 04                       mov al, [bp+4]         ; AL = low byte of x
24 ff                          and al, 0xFF           ; & 0xFF (AL form)
eb 00 5d c3                    epilogue
```

Findings:
- The cast `(unsigned char)(x & 0xFF)` folds to **AL-only byte ops**.
  BCC reads only the low byte of x via `mov al, [bp+4]` (opcode `8a`,
  byte form).
- **`and al, 0xFF` is NOT optimized away** even though it's a no-op
  on AL — BCC emits `24 ff` (2 bytes). The AL-accumulator form
  (opcode `24`) is 2 bytes vs `80 e0 ff` (3 bytes) generic-form.
- AH is left undefined; uchar return contract uses only AL.
- The signed/unsigned distinction at the param matters: this fn
  reads `int x` so it accesses [bp+4] as a byte (low half). If x
  were declared `unsigned char` instead, the param would still
  occupy a 16-bit slot but the body would be the same.


## `char *s = "hi"` — pointer-to-_DATA, NOT a copy

Fixture `2546-char-ptr-from-strlit-obj`:

```c
int main(void) {
  char *s;
  s = "hi";
  return s[1];
}
```

```
55 8b ec                       prologue
56                             push si
be 00 00                       mov si, 0          ; s = offset of "hi" in _DATA (FIXUPP)
8a 44 01                       mov al, [si+1]     ; s[1] = 'i'
98                             cbw                ; sign-extend char→int
eb 00 5e 5d c3                 epilogue
```

Findings:
- `char *s = "hi"` stores only the **pointer** to the string literal
  in `_DATA`. NO N_SCOPY@ call; no local byte copy. The literal
  lives at `_DATA[offset 0]` and s points to it.
- Critically distinct from `char s[] = "hi"` (`2509`), which:
  - reserves 4 bytes on the stack
  - calls N_SCOPY@ to copy the 3 bytes from _DATA → stack
- The two declarations have very different runtime cost and
  semantics: `char *s` is a pointer; `char s[]` is a local array.
- `s` here gets register promotion (`si`) since it's only used once
  after init.
- `s[1]` returns char → cbw promotes to int for the int return value.
- The "hi\0" literal is 3 bytes in `_DATA`; the FIXUPP relocates
  the `be 00 00` immediate to point to it.


## `char + char` — both promoted via cbw, push/pop stack spill

Fixture `2558-char-add-char-obj`:

```c
int main(void) {
  char a;
  char b;
  a = 10;
  b = 20;
  return a + b;
}
```

```
55 8b ec 4c 4c                 prologue + 2B local (2 chars in 2-byte slot)
c6 46 ff 0a                    a = 10           ; byte store at [bp-1]
c6 46 fe 14                    b = 20           ; byte store at [bp-2]
8a 46 ff                       mov al, a
98                             cbw              ; promote a
50                             push ax          ; SPILL a to stack
8a 46 fe                       mov al, b
98                             cbw              ; promote b
8b d0                          mov dx, ax       ; b → dx
58                             pop ax           ; restore a
03 c2                          add ax, dx       ; a + b (both int-promoted)
eb 00 8b e5 5d c3              epilogue
```

Findings:
- `char + char` requires integer promotion on BOTH operands BEFORE
  the add. Each cbw destroys the AH that the other operand might
  use, so the codegen has to save one operand somehow.
- **BCC's chosen pattern: push/pop stack spill.**
  - Load a → AX (`8a 46 disp + cbw`)
  - **Push AX**
  - Load b → AX, then mov to DX
  - **Pop AX** (restore a)
  - Add ax, dx
- More efficient alternatives exist (e.g. swap roles so first
  operand goes to BX or DX directly), but BCC's pattern is the
  "mechanical" one: always go through AX for each promotion, spill
  via stack between them.
- Char locals share a 2-byte slot at `[bp-1]` and `[bp-2]` — packed.
- This shape generalizes: any binary op on TWO chars (add, sub, etc.)
  pays this 4-byte overhead (push + pop + mov-dx) on top of the
  per-operand promotion.

