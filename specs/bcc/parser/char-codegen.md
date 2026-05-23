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


## `signed char c = -128` — `0x80` byte, cbw sign-extends to `0xFF80`

Fixture `2566-schar-neg-promote-obj`:

```c
int main(void) {
  signed char c;
  c = -128;
  return c + 1;
}
```

```
55 8b ec 4c 4c                 prologue + 2B local
c6 46 ff 80                    mov byte [bp-1], 0x80      ; c = -128
8a 46 ff                       mov al, c
98                             cbw                        ; sign-extend
40                             inc ax                     ; +1 peephole
eb 00 8b e5 5d c3              epilogue
```

Findings:
- `signed char c = -128` stores the byte **`0x80`** at `[bp-1]`
  (two's complement). The 0x80 byte has its sign bit set.
- `cbw` is the right tool: it takes AL = 0x80 and produces
  AX = 0xFF80 (= -128 as signed int). The sign extends correctly
  at the boundary value. This is the standard signed-char promote.
- `c + 1` uses the `inc ax` peephole (1 byte) — same as `int + 1`.
- Result: `0xFF80 + 1 = 0xFF81` = -127 (signed).
- Confirms the cbw promotion preserves value semantics across the
  signed-char range — no surprises at -128.
- Operator table holds:
  - Signed char promote: cbw (1B)
  - Unsigned char promote: mov ah, 0 (2B)


## `signed char arr[]` — bytes directly stored, no padding

Fixture `2593-signed-char-arr-obj`:

```c
signed char arr[3] = { -1, 0, 127 };
int main(void) {
  return arr[0] + arr[2];
}
```

`_DATA` bytes:
```
ff 00 7f       ; -1, 0, 127 (signed byte values)
```

Main body:
```
55 8b ec                       prologue
a0 00 00                       mov al, [_arr+0]    ; arr[0] = 0xFF
98                             cbw                 ; → ax = 0xFFFF (-1)
50                             push ax             ; spill
a0 02 00                       mov al, [_arr+2]    ; arr[2] = 0x7F
98                             cbw                 ; → ax = 0x007F (127)
8b d0                          mov dx, ax
58                             pop ax
03 c2                          add ax, dx          ; -1 + 127 = 126
eb 00 5d c3                    epilogue
```

Findings:
- `signed char arr[N]` initializer = N bytes packed in `_DATA`,
  one byte per element, no padding.
- Each `arr[K]` read uses `mov al, moffs8` (`a0` opcode) — direct
  AL load from a fixed memory offset, 3 bytes.
- Sign-extension via `cbw` matches signed-char convention.
  `0xFF` → `0xFFFF` (= -1), `0x7F` → `0x007F` (= 127).
- Sum uses the **push/pop spill pattern** (`2558`, `2563`) for two
  cbw-promoted operands.


## Global `char g = 'Z'` — 1-byte storage, `mov al, moffs8` load

Fixture `2657-global-char-obj`:

`_DATA` contents (1 byte): `5a` (= 'Z')

```
55 8b ec                       prologue
a0 00 00                       mov al, [_g]     (FIXUPP, moffs8 form 3B)
98                             cbw              (promote for int return)
eb 00 5d c3                    epilogue
```

Findings:
- Global `char` occupies **exactly 1 byte** in `_DATA` — NOT
  padded to 2 bytes. The next global would start at an odd offset.
- Reading uses **`a0 disp16`** (3 bytes) — the moffs8 form of
  "mov AL, [moffs16-addr]". Distinct from `a1 disp16` (mov AX,
  moffs16) used for int globals.
- `cbw` promotes for int-context return.


## `if (c == K)` for char — DIRECT byte memory compare

Fixture `2660-char-eq-int-obj`:

```c
int test(char c) {
  if (c == 'A') return 1;
  return 0;
}
```

```
55 8b ec                       prologue
80 7e 04 41                    cmp byte [bp+4], 'A'    (4B byte form!)
75 05                          jne → false
b8 01 00 eb 04                 true: ax = 1; jmp epi
33 c0 eb 00                    false: xor ax, ax
5d c3                          ret
```

Findings:
- `if (c == K)` with `char c` and char literal `K` uses the
  **`cmp byte ptr [mem], imm8`** form (`80 /7 mem imm8` = 4 bytes).
  NO cbw promote, NO load into a register.
- This is a **special-case optimization** for equality of byte
  values: BCC bypasses the int-promotion rule and uses byte ops.
- Contrast with `switch (c)` (`2655`) which DOES use cbw + int
  compares. Equality compare in an if-statement takes the
  efficient byte path; switch takes the int path.
- ModR/M `7e` = mod 01, opcode-ext 111 (cmp), r/m 110 (bp+disp8).
- This is a critical peephole for tight character-recognition
  code (parsers, lexers, etc.): `if (c == '\n')`, `if (c == '\t')`.


## `strlen` idiom `while (*s) { n++; s++; }` — compact while+byte-cmp loop

Fixture `2664-iter-string-obj`:

```c
int strlen_simple(const char *s) {
  int n = 0;
  while (*s) {
    n = n + 1;
    s = s + 1;
  }
  return n;
}
```

```
55 8b ec 56 57                 prologue + push si, di
8b 76 04                       mov si, s
33 ff                          xor di, di       ; n = 0
eb 0a                          jmp → COND
                               ; BODY:
8b c7 40 8b f8                 n = n + 1 (AX-acc, 5B)
8b c6 40 8b f0                 s = s + 1 (AX-acc, 5B)
                               ; COND:
80 3c 00                       cmp byte [si], 0
75 f1                          jnz → BODY
8b c7                          return n
eb 00 5f 5e 5d c3              epilogue
```

Findings:
- Classic C strlen idiom compiles to a tight test-at-bottom while.
- Both `n` and `s` register-promoted (di and si). Caller-saved.
- **`while (*s)` test = byte-direct `cmp byte [si], 0`** (3B,
  same as `2561`). No load to register, no cbw promote.
- The AX-acc pattern for both `n = n + 1` and `s = s + 1` costs
  10 bytes per iteration of body bookkeeping. Using `++n; ++s;`
  would shave 8 bytes off (2 × `inc reg`).
- This is a useful template for our reimpl's "string-scan" recognition.


## Char range check `c >= '0' && c <= '9'` — caches char in DL, byte cmp imm8

Fixture `2677-digit-check-obj`:

```c
int is_digit(char c) {
  if (c >= '0' && c <= '9') return 1;
  return 0;
}
```

```
55 8b ec                       prologue
8a 56 04                       mov dl, c              ; cache char in DL!
80 fa 30                       cmp dl, '0'            ; byte cmp (3B)
7c 0a                          jl → FALSE             ; signed branch
80 fa 39                       cmp dl, '9'
7f 05                          jg → FALSE
b8 01 00 eb 04                 ax = 1; jmp epi
33 c0                          FALSE: ax = 0
eb 00 5d c3                    epilogue
```

Findings:
- **Char range check caches `c` in DL** (the byte register) and
  reuses it across both compares. No re-load from memory.
- Each compare uses **`cmp dl, imm8`** = `80 fa imm8` (3 bytes) —
  the byte form. NO cbw promote, NO load into AX.
- Branches use signed `jl`/`jg` (char default-signed in BCC).
- 3-instruction setup + 5-instruction per arm = ~13 bytes for the
  whole range check. Very compact.
- Compare to int-version which would be 3B cmp + 2B jcc = same per
  compare. The char vs int byte-cmp form is the same length but
  skips the cbw.
- DL chosen over AL because AL is the "return value" register;
  DL is a free scratch.


## Char-to-char assignment — pure byte path (no promote/truncate)

Fixture `2685-char-param-to-local-obj`:

```c
int copy(char c) {
  char d;
  d = c;
  return d;
}
```

```
55 8b ec 4c 4c                 prologue + 2B local
8a 46 04                       mov al, c        (byte load)
88 46 ff                       [bp-1] = al      (byte store at odd offset)
8a 46 ff                       mov al, d
98                             cbw              (promote for int return)
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Char-to-char assignment uses **pure byte path**: `mov al, [src];
  mov [dst], al`. NO 16-bit promote-then-truncate.
- Local char stored at **odd `[bp-1]`** (single-byte at the top of
  the 2-byte local reserve, leaving [bp-2] unused for this case).
- Standard byte ops:
  - `mov al, [bp+disp]` = `8a 46 disp` (3B)
  - `mov [bp+disp], al` = `88 46 disp` (3B)
- `cbw` only at the final return where the value goes back to int
  context.
- This is the optimal char-handling shape: byte ops throughout,
  promote only at the int boundary.


## `signed char + unsigned char` — different scratch regs, NO push/pop spill

Fixture `2690-sc-vs-uc-promote-obj`:

```c
int both(signed char s, unsigned char u) {
  return s + u;
}
```

```
55 8b ec                       prologue
8a 46 04                       mov al, s
98                             cbw              ; signed → AX
8a 56 06                       mov dl, u
b6 00                          mov dh, 0        ; unsigned → DX (zero-extend)
03 c2                          add ax, dx
eb 00 5d c3                    epilogue
```

Findings:
- Signed char goes to AX via cbw; unsigned char goes to DX via
  `mov dh, 0`. **Different scratch registers avoid the push/pop
  spill** seen in `2558` (char + char where BOTH used AX).
- Net: 4-byte savings vs the double-cbw-AX case.
- Operand-pair routing for char arithmetic:
  - Two signed chars → both via AX (cbw twice) → push/pop spill
  - Two unsigned chars → probably similar with mov-ah-0 → spill
  - Mixed signed + unsigned → split via AX and DX (no spill)
- This is a subtle compile-time choice based on operand types.


## `(char)int` truncation cast — single `mov al, [mem]` (byte load = truncation)

Fixture `2707-int-to-char-cast-obj`:

```c
char trunc8(int x) {
  return (char)x;
}
```

```
55 8b ec                       prologue
8a 46 04                       mov al, [bp+4]    ; byte load = truncation
eb 00 5d c3                    epilogue
```

Findings:
- `(char)int` cast = **byte load of just the low byte**. Single
  `mov al, [bp+disp8]` (3 bytes via `8a 46 disp`). NO explicit
  truncation step (no `and ax, 0xFF`, no shift, nothing).
- The cast IS the byte load — addressing mode handles truncation
  automatically.
- Cast summary table:

| from → to            | bytes | instructions |
|----------------------|-------|--------------|
| `(int) int`          | 0B    | no-op |
| `(unsigned) int`     | 0B    | no-op (`2591`) |
| `(char) int`         | 3B    | byte load of low byte |
| `(int) char` signed  | 4B    | byte load + cbw |
| `(int) char` unsigned| 4B    | byte load + `mov ah,0` |
| `(long) int` signed  | 1B    | cwd |
| `(long) int` unsigned| 2B    | xor dx,dx |
| `(int) long`         | 3B    | load low word only |


## `int x = c;` (char→int implicit promotion) — byte load + cbw

Fixture `2793-char-asgn-int-obj`:

```c
int promote(char c) {
  int x = c;
  return x;
}
```

```
4c 4c                          dec sp twice (x)
8a 46 04                       mov al, c    (byte load)
98                             cbw          (sign-extend AL→AX)
89 46 fe                       x = ax
```

Findings:
- Implicit char→int conversion in assignment = **byte load + cbw**
  (4 bytes for the conversion).
- Same shape as explicit `(int)char` cast (`2538`).
- The conversion is signed by default (cbw, not `mov ah, 0`).
  For `unsigned char`, BCC would use `mov ah, 0` for zero-extension.


## `c >> 4` for char (promote first, then shift) — `cbw + sar ax, cl`

Fixture `2807-char-shr-4-obj`:

```c
int nibble(char c) {
  return (int)(c >> 4);
}
```

```
8a 46 04                       mov al, c    (byte load)
98                             cbw          (sign-extend AL→AX, char→int)
b1 04                          mov cl, 4
d3 f8                          sar ax, cl   (signed arithmetic shift)
```

Findings:
- C "integer promotion": char operand is promoted to int **before**
  the shift. The shift operates on the sign-extended int.
- Total 8 bytes for the operation: cbw (1B) + cl-form shift (4B) +
  byte load (3B).
- **`sar ax, cl`** opcode is `d3 f8` (signed arithmetic shift right).
- Compare to int `>> 4` which is 4 bytes (no cbw needed) for the
  shift portion; char adds 1 byte for the promotion.
- For unsigned char, BCC would emit `mov ah, 0` (zero-extend, 2B)
  instead of cbw, then `shr` (unsigned) instead of `sar`.


## `char id(char c) { return c; }` — minimal char identity (3-byte body)

Fixture `2824-char-arg-char-ret-obj`:

```
55 8b ec                       prologue
8a 46 04                       mov al, c       ; byte load (3B)
eb 00 5d c3                    epilogue
```

Findings:
- Char→char identity = **just `mov al, [bp+4]`** (3B).
- NO cbw (return is char, not int — AH is undefined per ABI).
- The minimal-size body for a non-trivial function: 3 bytes for
  the load, 5 bytes for prologue/epi.
- Compare to int→int identity (`8b 46 04` = `mov ax, [bp+4]`, also
  3B). Char and int identity are the same size — only the opcode
  differs (8a for byte, 8b for word).


## `signed char` parameter — byte-identical to plain `char`

Fixture `2838-signed-char-arg-obj`:

```c
int promote(signed char c) {
  return c;
}
```

```
8a 46 04                       mov al, c
98                             cbw
```

Findings:
- `signed char` is **byte-identical** to plain `char`. BCC's
  default `char` is signed.
- The `signed` qualifier on char is a no-op at codegen.
- Compare to `unsigned char` which would emit `mov ah, 0` instead
  of `cbw`.


## `c + 1` for char (returns int) — byte load + cbw + inc

Fixture `2871-char-plus-1-obj`:

```c
int succ(char c) {
  return c + 1;
}
```

```
8a 46 04                       mov al, c        (byte load)
98                             cbw              (char→int promote)
40                             inc ax           (+1 peephole)
```

Findings:
- Total 5 bytes for the expression.
- `+1` peephole (`inc ax`) still applies after cbw promotion.
- Sequence: byte load → promote → inc. Each step is its own
  byte-level op; no fancy combination.
- For `c + 2`, would use `inc ax; inc ax` (2B); for `c + K` with
  K >= 3, would use `add ax, K`.


## `unsigned char get(unsigned char c) { return c; }` — 3-byte body, no extension

Fixture `2881-uchar-ret-fn-obj`:

```
8a 46 04                       mov al, c
```

Findings:
- Unsigned char identity = same 3-byte body as signed char (`2824`).
- NO zero-extension to AH. Char return ABI doesn't define AH.
- The caller is responsible for any extension if needed.
- Char return is byte-identical for signed and unsigned variants.


## `++g` for char global — load+inc+store (8B, SUBOPTIMAL)

Fixture `2887-char-preinc-obj`:

```c
char g;
void bump(void) {
  ++g;
}
```

```
a0 00 00                       mov al, [_g]    (byte load via moffs8)
fe c0                          inc al          (byte inc reg)
a2 00 00                       mov [_g], al    (byte store)
```

Findings:
- `++char_global` = load + inc + store **via AL** (8 bytes total).
- **BCC misses the peephole**: could emit `inc byte [_g]` (`fe 06
  disp16`, 4B), which is the byte-mem-inc form. But BCC always
  goes through AL for char globals.
- Compare to `++int_global` (`2638`) which DOES use the mem-inc
  form (`ff 06 disp16`, 4B).
- Suboptimal 4B-extra cost per char-global increment.


## `char_global += 1` — same suboptimal 8-byte pattern as `++char_global`

Fixture `2891-char-plus-eq-1-obj`:

```c
char g;
void bump(void) {
  g += 1;
}
```

**Byte-identical body to `2887`** (`++g`):
```
a0 00 00 fe c0 a2 00 00
```

Findings:
- `g += 1` and `++g` produce **identical 8-byte bodies** for char
  globals.
- BCC misses the `inc byte [mem]` peephole for BOTH source forms.
- For int globals, both `g += 1` and `++g` get the 4B `inc word
  [mem]` peephole. For chars, neither does.
- Source-form choice doesn't help here — BCC's char-global path
  is just suboptimal.


## Local `char c; c += 1;` — DL register promotion, AL↔DL bounce

Fixture `2897-local-char-plus-eq-obj`:

```c
char c = 'A';
c += 1;
return c;
```

```
b2 41                          mov dl, 'A'      (c promoted to DL)
8a c2                          mov al, dl       (copy to AL for inc)
fe c0                          inc al
8a d0                          mov dl, al       (store back to DL)
8a c2                          mov al, dl       (reload for return)
98                             cbw
```

Findings:
- Local char gets **DL register promotion** in leaf fn (similar to
  int's si/di promotion but at byte level).
- Compound assignment is **inefficient**: bounces AL↔DL multiple
  times. Could be `inc dl; mov al, dl; cbw` (4B) but BCC emits
  ~6B of AL↔DL transfers.
- Total 11 bytes for the body. BCC's char codegen path is less
  polished than int.


## `char c >= 0` — `cmp byte [bp+disp], 0` direct (no promotion)

Fixture `2904-char-ge-zero-obj`:

```c
int nonneg(char c) {
  if (c >= 0) return 1;
  return 0;
}
```

```
80 7e 04 00                    cmp byte [bp+4], 0   (byte cmp, 4B)
7c 05                          jl → ELSE (signed < 0)
```

Findings:
- Char comparison with constant 0 uses **`cmp byte [mem], imm8`**
  form (`80 7e disp8 imm8`, 4B). NO int promotion needed.
- ModR/M `7e 04` = mod 01, r/m 110 = `[bp+disp8]`. Opcode `80` is
  `cmp r/m8, imm8`.
- Signed `>= 0` → `jl → ELSE` (`7c`, signed less-than).
- 4B+2B = 6B for the test — same size as int's optimized form.
- This shows char comparison can skip the cbw step entirely.


## `char c <= -1` — byte cmp with imm8 (0xFF as signed)

Fixture `2911-char-le-neg1-obj`:

```c
if (c <= -1) return 1;
return 0;
```

```
80 7e 04 ff                    cmp byte [bp+4], 0xFF   (= -1 as i8)
7f 05                          jg → ELSE               (signed > -1)
```

Findings:
- Char comparison with negative imm = `cmp byte [mem], 0xFF` (4B).
- Same byte-cmp form as char `>= 0` (`2904`); signed `<= -1` uses
  `jg` for the inverse branch.
- The 0xFF byte represents -1 in signed interpretation.
- No int promotion needed; byte-direct compare.


## Signed `char c >> 1` — byte load + cbw + `sar ax, 1` (6B)

Fixture `2955-schar-shr-obj`:

```c
int half(signed char c) {
  return c >> 1;
}
```

```
8a 46 04                       mov al, c
98                             cbw           (sign-extend char→int)
d1 f8                          sar ax, 1     (signed shift right)
```

Findings:
- Promote-then-shift: byte load + cbw + sar. 6 bytes total.
- `signed char` byte-identical to plain `char` (BCC default).
- For unsigned char `>> 1`, would be `mov al; mov ah, 0; shr ax, 1`
  (5B without the `mov ah, 0` if already known zero).


## `char a < char b` — byte-level `cmp al, [mem]` (no promotion)

Fixture `2984-char-lt-char-obj`:

```c
int lt(char a, char b) {
  if (a < b) return 1;
  return 0;
}
```

```
8a 46 04                       mov al, a
3a 46 06                       cmp al, byte [bp+6]   (BYTE cmp, 3a /r form)
7d 05                          jge → FALSE
```

Findings:
- Char-to-char comparison uses **byte-level `cmp al, [mem]`**
  (`3a 46 disp8`, 3B). NO cbw promotion needed!
- ModR/M `46 disp8` for `[bp+disp8]`. Opcode `3a /r` = `cmp r8, r/m8`.
- 3 bytes total for the cmp — same as int cmp `3b 46 disp` form
  but without the cbw prep.
- For mixed char/int comparison, BCC would promote (per usual
  arith conversions).


## `unsigned char + int` — `mov ah, 0` zero-extend (NOT cbw)

Fixture `3018-uchar-plus-int-obj`:

```c
int mix(unsigned char c, int n) {
  return c + n;
}
```

```
8a 46 04                       mov al, c
b4 00                          mov ah, 0   (UNSIGNED zero-extend, 2B)
03 46 06                       add ax, n
```

Findings:
- Unsigned char to int = **`mov ah, 0`** (2 bytes) — zero-extend.
- Signed char to int = **`cbw`** (1 byte) — sign-extend.
- **Unsigned promotion is 1 BYTE LONGER** than signed.
- Same value range for non-negative chars, but the codegen differs.

## `(unsigned char)int` cast — single `mov al, [mem]` (no zero-ext for char return)

Fixture `3019-uchar-cast-obj`:

```c
unsigned char low(int x) {
  return (unsigned char)x;
}
```

```
8a 46 04                       mov al, x   (3B byte load only)
```

Findings:
- Single byte load = the result. AH not cleared (don't care for
  char return — caller treats AL as the uchar value).
- For an `int` return of uchar value, would need `mov ah, 0`.


## `char_g += 1` global — load AL + inc al + store AL (8B, MISSES `inc byte [mem]`)

Fixture `3063-char-global-plus-eq-obj`:

```c
char tick;
tick += 1;
```

```
a0 00 00                       mov al, [_tick]   (byte load, 3B)
fe c0                          inc al            (8-bit inc, 2B)
a2 00 00                       mov [_tick], al   (byte store, 3B)
```

**Total: 8 bytes**.

Findings:
- Char compound assign uses byte load + 8-bit inc + byte store.
- **Missed peephole**: 8086 supports `inc byte [mem]`
  (`fe 06 disp16`, 4B+FIXUPP). BCC does NOT use it.
- Word global `g += 1` = 4B (`inc word [mem]`).
- Char global `g += 1` = 8B — **4 bytes longer than int** due to
  the missing mem-byte-inc peephole.
- Same suboptimality for `++char_g` and `char_g -= 1`.


## `char c * 2` — STRENGTH-REDUCED after cbw promotion

Fixture `3104-char-mul-2-obj`:

```c
int dbl(char c) {
  return c * 2;
}
```

```
8a 46 04                       mov al, c
98                             cbw
d1 e0                          shl ax, 1     (strength-reduced!)
```

Findings:
- `char * 2` = byte load + cbw + `shl ax, 1` (6 bytes total).
- Multiplication is always safe to strength-reduce (no rounding).
- Same general pow2-mul rule applies after char promotion.


## `unsigned char c == 'A'` — BYTE-IDENTICAL to signed char compare

Fixture `3131-uchar-eq-A-obj`:

```c
unsigned char c == 'A'    /* byte-identical to signed char */
```

```
80 7e 04 41                    cmp byte [bp+4], 0x41
75 05                          jne → FALSE
```

Findings:
- Signedness doesn't matter for `==` — both halves are raw bytes.
- Same byte-cmp peephole as `3130`.

## `char c != '\0'` — `cmp byte [mem], 0; je → FALSE`

Fixture `3132-char-ne-nul-obj`:

```
80 7e 04 00                    cmp byte [bp+4], 0
74 05                          je → FALSE
```

Findings:
- NUL test on char param = byte-cmp-vs-zero + je-to-FALSE (4B + 2B).

## Char range check `c >= '0' && c <= '9'` — uses DL, not AL

Fixture `3133-digit-range-obj`:

```c
if (c >= '0' && c <= '9') return 1;
```

```
8a 56 04                       mov dl, c   (DL register!)
80 fa 30                       cmp dl, '0' (0x30)
7c 0a                          jl → FALSE
80 fa 39                       cmp dl, '9' (0x39)
7f 05                          jg → FALSE
                               ; TRUE
```

Findings:
- Char range test loads param into **DL** (not AL) to keep AX free
  for return value or other operations.
- ModR/M `fa` = mod 11 op-ext 111 (cmp /7) r/m 010 (DL).
- 3B per byte-cmp, 2B per signed jump — 13 bytes for the test.
- Signed `jl`/`jg` since char is signed by default.

## `signed char c < 0` — byte cmp + `jge → FALSE` (no cbw)

Fixture `3136-signed-char-neg-obj`:

```c
if (c < 0) return 1;
```

```
80 7e 04 00                    cmp byte [bp+4], 0
7d 05                          jge → FALSE
```

Findings:
- Direct byte cmp + signed jge — no cbw promotion needed.
- 6 bytes total for the test.


## Unsigned char compare — uses unsigned jumps (`jb`/`jae`/`ja`/`jbe`)

Fixture `3137-uchar-lt-FF-obj`:

```c
unsigned char c < 0xFF   /* jae → FALSE for >=, since signedness differs */
```

```
80 7e 04 ff                    cmp byte [bp+4], 0xFF
73 05                          jae → FALSE  (UNSIGNED ≥)
```

Findings:
- Char compare jump table:
  - signed char `<`: `jl` (`7c`)
  - **unsigned char `<`: `jb` (`72`)**
  - signed char `>=`: `jge` (`7d`)
  - **unsigned char `>=`: `jae` (`73`)** ← here
- BCC honors signedness even at byte-cmp level.

## Global char `tag == 0` — direct `cmp byte [mem], 0` (5B + FIXUPP)

Fixture `3138-char-global-eq-0-obj`:

```c
char tag;
if (tag == 0) return 1;
```

```
80 3e 00 00 00                 cmp byte [_tag], 0   (5B with FIXUPP)
75 05                          jne → FALSE
```

Findings:
- Global char compare uses **direct mem-byte cmp** (`80 /7 disp16 imm8`).
- ModR/M `3e` = mod 00, cmp op-ext (/7), r/m 110 (disp16).
- 5 bytes for the compare. NO load to AL first.


## `char c <<= 1` local — `shl dl, 1` (byte shift, uses DL not AL)

Fixture `3151-char-shl-eq-1-obj`:

```c
char c;
c <<= 1;
return c;
```

```
8a 56 04                       mov dl, c       (loaded into DL!)
d0 e2                          shl dl, 1       (8-bit shift, 2B)
8a c2                          mov al, dl      (transfer to AL)
98                             cbw             (promote for int return)
```

Findings:
- Byte shift uses `d0 /op` opcode form for 8-bit shifts.
- ModR/M `e2` = op-ext 100 (shl), r/m 010 (DL).
- Loaded into DL not AL — likely so cbw doesn't clobber the value
  pre-shift.
- 8 bytes total: 3B mov + 2B shl + 2B mov + 1B cbw.


## `if (!c)` for char — promote to int first, then test

Fixture `3204-not-char-obj`:

```c
if (!c) return 1;
```

```
8a 46 04                       mov al, c
98                             cbw                  (promote)
0b c0                          or ax, ax            (test == 0 peephole)
75 05                          jne → FALSE
```

Findings:
- The `!` operator triggers int promotion before testing.
- 8 bytes total (vs 6B for byte-direct `c != '\0'` cmp).
- Compare to `char == 0` (`3132`) which uses direct `cmp byte [mem], 0`
  (4B + 2B jump). Source form `if (!c)` is 2 bytes longer.

## `char c + 1` — `mov al + cbw + inc ax` (5B, uses inc not add)

Fixture `3206-char-plus-const-obj`:

```c
return c + 1;
```

```
8a 46 04 98                    mov al, c; cbw
40                             inc ax        (1B!)
```

Findings:
- `char + 1` uses `inc ax` (1B) — saves over `add ax, 1` (3B).
- 5 bytes total.
- For `char + K` with K > 1, would use `add ax, K`.


## `char c >>= 1` local — `sar dl, 1` (signed 8-bit shift)

Fixture `3226-char-shr-eq-1-obj`:

```c
char c;
c >>= 1;
```

```
8a 56 04                       mov dl, c
d0 fa                          sar dl, 1     (signed byte shift)
8a c2                          mov al, dl
98                             cbw
```

Findings:
- ModR/M `fa` = mod 11, op-ext 111 (sar /7), r/m 010 (dl).
- Signed char uses `sar`; for `unsigned char` would use `shr` (op-ext /5).
- Uses DL register (same as `<<=` `3151`).
- 8 bytes total: 3B load + 2B shift + 2B mov + 1B cbw.


## `++c` for char param (returned as int) — register round-trip suboptimal

Fixture `3273-pre-inc-char-param-obj`:

```c
return ++c;
```

```
8a 56 04                       mov dl, c      (load to DL per char-param convention)
8a c2                          mov al, dl     (copy to AL)
fe c0                          inc al         (byte inc)
8a d0                          mov dl, al     (copy back to DL — REDUNDANT)
98                             cbw            (promote to int)
```

Findings:
- 10 bytes total.
- BCC's char-param convention puts the value in DL.
- For modification like `++c`, BCC does round-trip: DL → AL → inc → DL.
- The `mov dl, al` post-inc is redundant (no further use of DL).
- Naive `mov al, c; inc al; cbw` would be 6B — BCC wastes 4 bytes here.
- Suboptimal but consistent with BCC's register-allocation strategy.


## char + int — char promoted via `cbw`

Fixture `3344-char-plus-int-obj`:

```c
char c = 5;
int add(int n) { return c + n; }
```

```
a0 00 00 [FIXUPP _c]           mov al, [_c]
98                             cbw              (sign-extend to int)
03 46 04                       add ax, n
```

Findings:
- Global `char` promoted to int via `cbw` (1B) before mixed arithmetic.
- Note: BCC default char is signed (cbw, not zero-ext).

## char param compared with int literal (fits in byte) — direct byte cmp

Fixture `3345-char-cmp-lit-obj`:

```c
int is_a(char c) { if (c == 65) return 1; return 0; }
```

```
80 7e 04 41                    cmp byte [bp+4], 65
75 05                          jne ELSE
```

Findings:
- `c == 65` compiles as direct byte cmp (`80 /7 r/m8, imm8`) — no widening.
- 4-byte cmp instruction (1B opcode + 1B ModR/M+disp + 1B disp + 1B imm = 4B for [bp+disp8]).
- Saves vs widening path: would be `mov al, [bp+4]; cbw; cmp ax, 65` (~6B).


## Escape sequences resolved at lexer/parser stage

Fixtures `3383-char-newline-obj`, `3384-char-hex-esc-obj`, `3385-char-octal-esc-obj`:

```c
c == '\n'      ; → cmp byte, 0x0a
c == '\x41'    ; → cmp byte, 0x41 (= 'A')
c == '\012'    ; → cmp byte, 0x0a (= '\n', octal 012 = decimal 10)
```

All three produce a 4-byte `cmp byte [bp+disp], imm8` form with the resolved literal.

Findings:
- `\n` → 0x0a, `\xNN` → hex byte, `\NNN` → octal byte.
- All escapes resolved at lex/parse time — no runtime indirection.
- Identical OBJ for `'\n'`, `'\012'`, and `(char)10`.

## Multi-char literal `'AB'` — first char in LOW byte (LE)

Fixture `3386-multichar-lit-obj`:

```c
int magic(void) { return 'AB'; }
```

```
b8 41 42                       mov ax, 0x4241    ('A' = 0x41 in AL, 'B' = 0x42 in AH)
```

Findings:
- BCC stores first char in the LOW byte (LSB-first ordering).
- This matches the natural little-endian byte order on 8086.
- Differs from GCC's convention (which stores `'AB' = 0x4142`, MSB-first).


## `char c == -1` — direct byte cmp with 0xff

Fixture `3442-char-eq-neg1-obj`:

```
80 7e 04 ff                    cmp byte [bp+4], 0xff
75 05                          jne ELSE
```

Findings:
- `-1` fits in a signed byte (sign-extends to 0xff in byte representation).
- Direct byte-cmp form (4B) — no widening needed.
- Matches the pattern from 3345 (char == 65).


## `char + char` returning char — direct byte arithmetic, no widening

Fixture `3517-char-add-char-obj`:

```c
char sum(char a, char b) { return a + b; }
```

```
8a 46 04                       mov al, a
02 46 06                       add al, b
```

Findings:
- 6B body. Pure byte ops via `add r8, r/m8` (opcode 0x02).
- No `cbw` widening since both operands and result are char.
- Result in AL — char return convention.


## char swap — 1-byte t but 2-byte stack alloc

Fixture `3529-char-swap-obj`:

```c
void cswap(char *a, char *b) {
  char t = *a;
  *a = *b;
  *b = t;
}
```

```
4c 4c                          dec sp; dec sp   (2B alloc even though t is 1B)
56 57                          push si; push di
8b 76 04                       mov si, a
8b 7e 06                       mov di, b
8a 04                          mov al, [si]     (t = *a)
88 46 ff                       mov [bp-1], al   (store t as byte)
8a 05                          mov al, [di]     (*b)
88 04                          mov [si], al     (*a = *b)
8a 46 ff                       mov al, [bp-1]   (load t)
88 05                          mov [di], al     (*b = t)
```

Findings:
- BCC's stack alloc unit is 2B even for char locals.
- char t stored at [bp-1] (only 1 byte used; [bp-2] wasted).
- 19B body.


## `(unsigned char)c` — no-op (3B byte load only)

Fixture `3537-char-to-uchar-obj`:

```c
unsigned char to_unsigned(char c) { return (unsigned char)c; }
```

```
8a 46 04                       mov al, c
```

Findings:
- Bit-identical reinterpretation. No widening or extension.
- 3B body. Cast is purely a type annotation; no codegen.

