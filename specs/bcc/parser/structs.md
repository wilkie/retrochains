# Structs, unions, members, typedefs

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## `while(1)` = unconditional jmp at bottom; `do {} while(0)` = body once no loop; partial init zero-fills; nested struct flat layout

Fixtures `2321`-`2326` cover const-condition
elision and aggregate initialization padding.

- `2321` (**`while(1)`**): no test, just an
  unconditional jmp back to body. Loop only
  exits via `break`:
  ```
  body:
    inc si                  ; i++
    cmp si, 5
    jl skip                 ; if i < 5 continue
    jmp end                  ; break
  skip:
    jmp body                 ; unconditional loop-back
  end:
  ```
- `2322` (**`do {} while(0)`**): body emitted
  once, NO loop-back. The const-false test elides
  the loop entirely:
  ```
  mov si, 5                 ; x = 5
  inc si                    ; x++ (body runs once)
  ; (no test, no jmp)
  ```
  This is the canonical `do { ... } while(0)`
  idiom used in macros to force statement-block
  semantics — BCC reduces it to inline code.
- `2323` (**partial array init zero-fill**):
  ```
  ; int a[10] = {1, 2, 3};
  _DATA:
    [01 00] [02 00] [03 00]   ; explicit init
    [00 00] [00 00] [00 00]   ; zero-fill the rest
    [00 00] [00 00] [00 00] [00 00]
  ```
  Per C standard: any uninitialized aggregate
  members are zero-initialized. 20 bytes total in
  `_DATA`.
- `2324` (**union of struct and long**): both
  members share the SAME storage. Writing to
  union.parts.lo/hi populates the same bytes
  that union.whole would read as a long:
  ```
  ; u.parts.lo at [bp-4] = 0x1234
  ; u.parts.hi at [bp-2] = 0x5678
  
  ; Reading u.whole >> 16 = high 16 bits
  ; = 0x5678 (just load offset -2)
  ```
  No struct/union-specific instructions; pure
  memory aliasing.
- `2325` (**`char s[10] = "abc"`**):
  ```
  _DATA:
    [61 62 63 00]            ; "abc\0"
    [00 00 00 00 00 00]      ; zero-fill rest
  ```
  String init places the literal + null, then
  pads with zeros to declared size.
- `2326` (**nested struct init**): members laid
  out flat in memory order:
  ```
  ; struct Outer { struct Inner { int a; int b; } i; int c; };
  ; static struct Outer o = {{10, 20}, 30};
  
  _DATA:
    [0a 00]                  ; o.i.a at offset 0
    [14 00]                  ; o.i.b at offset 2
    [1e 00]                  ; o.c   at offset 4
  ```
  Access via direct offsets: `mov ax, [o]`,
  `add ax, [o+2]`, `add ax, [o+4]`.

**Aggregate initialization rules (final, BCC 2.0)**:
| Form | Behavior |
|------|----------|
| `int a[N] = {a, b, c}` | First N init'd, rest zero-fill |
| `int a[] = {a, b, c}` | Size = number of inits |
| `char s[N] = "..."` | String + \0, padded w/ zeros to N |
| `char s[] = "..."` | Size = strlen + 1 |
| `struct {} s = {a, b, c}` | Members in order |
| Nested `{{...}, ...}` | Recursive per-member |
| Static: in `_DATA` (init) or `_BSS` (zero) | |
| Auto (non-static): `N_SCOPY@` from `_DATA` template | |

**Const-condition elision rules (final)**:
| Source | Effect |
|--------|--------|
| `if (1)` | Body emitted; no test |
| `if (0)` | Body NOT emitted (or jmp-over emitted but body still in OBJ) |
| `while (1)` | Body + unconditional jmp back at bottom |
| `while (0)` | Loop entirely elided |
| `do {} while (1)` | Body + unconditional jmp back |
| `do {} while (0)` | Body once, NO loop-back |
| `for (;;) ` | Same as while(1) |
| `1 && X` | Right operand evaluated |
| `0 && X` | jmp over X (X compiled but dead) |
| `1 || X` | jmp over X (X compiled but dead) |
| `0 || X` | Right operand evaluated |
| `1 ? A : B` | A evaluated, B elided |
| `0 ? A : B` | B evaluated, A elided |

For the Rust reimplementation:
- Aggregate init: zero-fill the rest, emit
  template to `_DATA`.
- Nested struct: flatten via member offsets.
- Union: alias storage; same byte addresses for
  all members.
- const-condition elision: special-case at parse
  time for the patterns in the table above.

## `fn()` ≡ `fn(void)` in BCC codegen; self-ref struct via fwd ptr; `0 && ...` jumps OVER (not elided); arr[0] vs ->y same access; fn-ptr param call via `ff /2 [bp+disp]`

Fixtures `2309`-`2314` cover assorted constructs.

- `2309` (**`fn()` ≡ `fn(void)`**): byte-identical
  function bodies. K&R `()` syntax accepted; same
  codegen as ANSI `(void)`. (Strict ANSI mode
  with `-A` may differ at parser level for
  diagnostics, but codegen unchanged.)
- `2310` (**self-referencing struct**): `struct
  Node { ...; struct Node *next; };` — works
  because the pointer's size is known regardless
  of pointee. Static init `{1, &n2}` produces a
  FIXUPP for the `next` field pointing to n2:
  ```
  _DATA layout:
    [02 00] [00 00]            ; n2: v=2, next=NULL
    [01 00] [&n2 FIXUPP]        ; n1: v=1, next=&n2
  
  ; n1.v + n1.next->v
  mov bx, [n1.next]            ; = &n2
  mov ax, [n1.v]                ; = 1
  add ax, [bx]                  ; += n2.v
  ```
- `2311` (**`0 && X` short-circuit**): BCC emits
  an unconditional `jmp` OVER the right operand
  (which is still compiled into the binary, just
  unreachable at runtime):
  ```
  ; r = (0 && side_effect(&x));
  mov word [x], 0
  jmp +17                       ; skip right operand
  
  ; (skipped at runtime, but still in OBJ):
  lea ax, [x] / push ax
  call _side_effect
  ; ...
  
  ; r = 0 (the && result when left is false)
  ```
  So this is **runtime short-circuit** (not DCE).
  The right operand's machine code is still
  emitted, just bypassed by the `jmp`.
- `2312` (**`extern int arr[];`**): incomplete-
  type external. EXTDEF emitted; size unknown to
  this TU. Access via FIXUPP to linker-resolved
  address.
- `2313` (**`.` direct vs `->` ptr-deref**):
  - `p.x` (static p): direct memory access via
    `a1 disp16` (FIXUPP'd)
  - `pp->y` (through ptr): `mov ax, [bx+disp]`
    where bx holds the pointer
  Same final access mechanism, just different
  base-address computation.
- `2314` (**fn ptr as parameter**): callee calls
  through stack arg:
  ```
  ; In apply(int (*f)(int), int x):
  push word [bp+6]              ; x
  ff 56 04                     ; call near [bp+4]  (= f)
  pop cx                        ; cleanup
  ```
  ModR/M `56 disp8` = /2 = call near indirect
  through [bp+disp8].

**Short-circuit & const-folding interaction**:
- `0 && X`: emits jmp over X's evaluation
- `1 || X`: emits jmp over X's evaluation
- `1 && X`: emits X's evaluation (no skip needed
  since condition is needed)
- `0 || X`: emits X's evaluation
- For NON-const operands: standard short-circuit
  branching with cmp+jcc

Note that BCC compiles the right operand even
when it's known unreachable, then emits a jmp
over it. This is wasteful in code size but
simple — no DCE pass.

**Function-pointer call-site forms** (complete):
| Source | Encoding |
|--------|----------|
| `f(args)` (direct fn call) | `e8 [rel]` near, `9a [rel][seg]` far |
| `(*fp)(args)` or `fp(args)` (single ptr local) | `ff 56 disp` ([bp+disp]) |
| `fns[0](args)` (static array const idx) | `ff 16 disp16` (direct indirect) |
| `fns[i](args)` (var idx) | `ff 97 disp16` ([bx+disp16]) |
| `s->fn(args)` (via struct field ptr) | `ff /2 [bx+disp]` or `ff 16 disp16` |

For the Rust reimplementation:
- Self-ref struct: track forward-declared type
  for pointer field resolution.
- Const-folded short-circuit: emit unconditional
  jmp over dead operand (still compile it).
- Fn ptr in stack arg: emit `ff 56 disp` for
  `[bp+disp]` indirect.

## Inline asm w/ C vars; static fn-ptr arr = FIXUPP'd ptrs in _DATA; typedef parse-only; enum const-folded; var-idx fn-ptr call via `ff /2 [bx+disp]`

Fixtures `2303`-`2308` cover inline asm, typedef
chains, enums, and fn-ptr arrays.

- `2303` (**asm simple mov**): the asm body emits
  directly inline. No register save/restore:
  ```
  push bp / mov bp, sp
  mov ax, 42                ; from `asm { mov ax, 42; }`
  xor ax, ax                ; from `return 0;` (overwrites asm's ax)
  ; epilogue
  ```
- `2304` (**asm with C variable**): C local
  variables in asm get translated to BP-relative
  ModR/M:
  ```
  ; asm { mov ax, x; add ax, 5; mov x, ax; }
  ; where x is at [bp-2]
  
  mov ax, [bp-2]            ; 8b 46 fe
  add ax, 5                  ; 05 05 00
  mov [bp-2], ax            ; 89 46 fe
  ```
- `2305` (**static fn-ptr array, indexed-const
  calls**): `_DATA` contains 3 FIXUPP'd near
  pointers. Calls via `ff 16 disp16` (direct
  indirect):
  ```
  _DATA: [_a addr][_b addr][_c addr]  ; 6 bytes, 3 FIXUPPs
  
  ff 16 00 00                ; call near [&fns[0]]
  ff 16 02 00                ; call near [&fns[1]]
  ff 16 04 00                ; call near [&fns[2]]
  ```
- `2306` (**typedef chain**): pure parser alias.
  `Int1 = int`, `Int2 = Int1`, ... all collapse to
  `int`. Byte-identical to plain `int a, b, c`.
- `2307` (**enum with mixed init**): values
  resolved at parse time. Implicit values
  continue from the last explicit (B = A+1, D =
  C+1, E5 = D+1):
  ```
  enum E { A = 1, B, C = 10, D, E5 };
  ;       A=1, B=2, C=10, D=11, E5=12
  ; A + B + C + D + E5 = 36, folded to mov ax, 36
  ```
- `2308` (**var-idx fn-ptr call**): for `ops[i]()`
  with variable i:
  ```
  ; Compute index × sizeof(ptr):
  mov bx, si                 ; i
  shl bx, 1                  ; × 2 (near ptr size)
  
  ; Indirect call through [bx + ops_base]:
  ff 97 NN NN                ; call near [bx+disp16]
                              ; ModR/M /2 rm=111 = [BX+disp16]
  ```

**Inline asm summary**:
| Pattern | BCC handling |
|---------|--------------|
| `asm { mov ax, 42; }` | Emits `mov ax, 42` literally |
| `asm { mov ax, x; }` (C local) | Translates `x` → `[bp+disp]` |
| `asm { mov ax, global; }` | Translates → direct disp16 |
| Labels inside asm | (limited; see fixture 2123 elision) |
| Multiple instructions | All emitted sequentially |
| Register modification | NOT saved/restored around block |

**Type-alias mechanisms** (all parser-only):
- `typedef T NAME` — alias for an existing type
- Forward-declared structs / unions
- enum tags vs enum values (values are int consts)
- All collapse to underlying type for codegen

**Function-pointer arr call forms**:
| Form | Encoding |
|------|----------|
| `fns[0]()` (const idx) | `ff 16 disp16` (direct indirect) |
| `fns[i]()` (var idx) | `ff 97 disp16` ([bx+disp16] indirect) |
| `(*fp)()` (single fp) | `ff /2 [bp+disp]` (local var) |
| `fp()` (single fp) | (same as above, * is implicit) |

For the Rust reimplementation:
- Inline asm: pass through directly; translate C
  var names to ModR/M addressing.
- Typedef: collapse during AST construction.
- Enum: store as int constants in the symbol table.
- Var-idx fn ptr call: compute index×sizeof in
  reg, emit `ff 97 disp16`.

## `typedef fn ptr` is parse-time alias; multi-fn TU = one _TEXT seg + per-fn PUBDEF; extern fn = EXTDEF + FIXUPP'd call

Fixtures `2252` (typedef fn ptr), `2253` (5 fns
in one TU), `2254` (extern decl no body) cover
function-level translation-unit organization.

- `2252` (**`typedef int (*BinOp)(int,int)`**):
  pure parse-time alias. Codegen identical to
  using the raw type:
  ```
  ; In apply(BinOp f, int a, int b):
  push word [bp+8]                ; b
  push word [bp+6]                ; a
  ff 56 04                        ; call near [bp+4] — through f
  ```
- `2253` (**multi-fn same TU**): all fns share
  one `_TEXT` segment in the small model. Each
  fn gets its own PUBDEF entry. Bodies emitted
  in declaration order; PUBDEF emission order
  appears to be based on internal symbol table
  layout (not strict declaration order). The
  caller's relative calls (`e8 [rel]`) are filled
  in directly at compile time since all targets
  are intra-segment.
- `2254` (**extern fn decl no body**): only an
  EXTDEF entry; no PUBDEF, no body. Call sites
  use FIXUPP'd `e8 00 00`:
  ```
  e8 00 00                ; call near (rel16)
                          ; FIXUPP relocates the rel16 at link time
  ```
  Linker assumes the extern target ends up in
  the same code segment as the caller (small/
  compact model). For medium/large/huge with
  extern, would use `9a [off][seg]` full far
  call instead.

**Translation-unit symbol summary**:
| Declaration | PUBDEF | EXTDEF | Body | Notes |
|-------------|--------|--------|------|-------|
| Defined globally | ✓ | ✗ | ✓ | Exported |
| Defined `static` | ✗ | ✗ | ✓ | Local-only |
| `extern` declared, used | ✗ | ✓ | ✗ | Linker resolves |
| `extern` declared, unused | ✗ | ✗ | ✗ | Elided |
| `typedef` | ✗ | ✗ | ✗ | Parse-time only |

**Multi-fn TU emission order**:
- Bodies in `_TEXT`: declaration order
- PUBDEFs: appears to be hash-table iteration
  order (not strict declaration order)
- EXTDEFs: appears in use-order

For the Rust reimplementation:
- Maintain a per-TU symbol table.
- Emit one `_TEXT` segment per TU containing all
  global fn bodies in declaration order.
- Emit PUBDEFs for non-static globals.
- Emit EXTDEFs for referenced undefined symbols.
- Treat typedef as a parse-time alias only.

## Typedef-arr transparent; struct w/ ptr field init = FIXUPP to inline string; arr-of-struct flat layout

Fixtures `2099` (typedef array), `2100` (struct
with ptr field init), `2101` (array of struct)
exercise composite-type initializers.

- `2099` (**typedef array transparent**):
  ```c
  typedef int IntArr5[5];
  static IntArr5 a = {1,2,3,4,5};
  ```
  Byte-identical to `static int a[5] = {...}`.
  Typedef of array types is fully transparent at
  codegen.
- `2100` (**struct with `char *name`**): the
  string literal is placed in `_DATA` right
  after the struct, and the struct's ptr field
  is initialised via FIXUPP to point to it:
  ```
  ; _DATA layout:
  offset 0..1:  ptr_to_apple (FIXUPP, resolves to offset 4)
  offset 2..3:  qty (= 5)
  offset 4..9:  "apple\0"
  ```
  So `{"apple", 5}` emits the struct followed by
  the inline string literal. The ptr is a 2-byte
  near offset.
- `2101` (**array of struct flat layout**):
  ```c
  static struct P pts[3] = {{1,2}, {3,4}, {5,6}};
  ```
  Data emits `01 00 02 00 03 00 04 00 05 00 06
  00` (12 bytes for 3 structs × 4 bytes). Array
  stride = sizeof(struct) = 4. Field access:
  - pts[i].x at offset i*4
  - pts[i].y at offset i*4 + 2

**Composite-init summary**:
| Form | Layout |
|------|--------|
| `typedef T arr[N]; static T a[]=...;` | Same as direct array (transparent) |
| `static struct {char *p; int n;} s = {"x", 5}` | struct then inline string; ptr FIXUPP'd |
| `static struct S arr[N] = {...}` | N structs flat row-major; stride = sizeof(struct) |

For the Rust reimplementation:
- Track typedef'd array types; emit as primitive array.
- Struct with string literal field: emit struct bytes, then string, with FIXUPP for the ptr.
- Array of struct: emit struct-stride sequence.

## `char s[3]="abc"` drops `\0` (classic C trap); larger arr zero-pads; struct partial init zero-pads

Fixtures `2096` (char s[3]="abc"), `2097` (char
s[6]="ab"), `2098` (struct partial init) refine
the static-init rules.

- `2096` (**`char s[3] = "abc"` drops null**):
  data emits just `61 62 63` (3 bytes). The
  null terminator is **dropped** when the declared
  size matches the string length without room
  for it. **Classic C trap** — programmers who
  write `char s[3] = "abc"` lose the null.
- `2097` (**`char s[6] = "ab"` zero-pads
  including null**): data emits `61 62 00 00 00
  00` (6 bytes). "ab" + null at index 2 +
  three more zero bytes for padding.
- `2098` (**struct partial init**): `static
  struct S s = {10, 20};` (3-int struct) data
  emits `0a 00 14 00 00 00` (6 bytes). Like
  array partial init — unmentioned fields are
  **zero-padded**.

**Static-init filling summary**:
| Init form | Declared size | Result |
|-----------|---------------|--------|
| `int arr[N] = {a, b, ...}` (M ≤ N items) | N | first M filled, rest zero |
| `int arr[] = {a, b, ...}` | M | exactly M items |
| `char s[N] = "...str..."` (strlen+1 ≤ N) | N | string + null + zero-pad |
| `char s[N] = "...str..."` (strlen == N) | N | string only, **null dropped** |
| `char s[] = "...str..."` | strlen+1 | string + null |
| `struct S s = {a, b, ...}` | sizeof(struct) | first fields filled, rest zero |

For the Rust reimplementation:
- Static char-array init with exact-size: drop null.
- Static array partial init: zero-pad tail.
- Static struct partial init: zero-pad unmentioned
  fields.

## `register` enregisters into SI/DI; typedef = type-only no codegen; enum = parse-time int consts

Fixtures `2069` (register), `2070` (typedef),
`2071` (enum) finish the type-system survey.

- `2069` (**`register int`**): two `register
  int` declarations get enregistered into SI
  and DI:
  ```
  be 05 00                ; mov si, 5 (x in SI)
  bf 0a 00                ; mov di, 10 (y in DI)
  mov ax, si / add ax, di
  ```
  For simple cases (≥2 reads, ≤2 register-eligible
  locals), BCC would enregister anyway, so the
  effect is mostly a hint. May make a difference
  with many candidate locals.
- `2070` (**`typedef`**): byte-identical to using
  the base type directly. **Purely type-system**;
  no codegen effect. `typedef int mytype; mytype
  x = 10;` ≡ `int x = 10;`.
- `2071` (**`enum`**): enum values are **parse-
  time integer constants** (like `#define` but
  type-checked):
  - `RED` = 0, `GREEN` = 1, `BLUE` = 2 by default
  - `enum color c = GREEN` stores `1` (2-byte int)
  - `c * 10 + RED + BLUE` folds the constants
    `0 + 2 → 2` at parse time
  - Result emits as `c * 10 + 2` with `inc ax /
    inc ax` (`40 40`, 2 bytes) for the +2.

**Small-constant add optimisation**:
| Adjust | Encoding | Bytes |
|--------|----------|-------|
| +1 | `inc reg` | 1 |
| +2 | `inc reg / inc reg` | 2 |
| +3 to +127 | `add reg, imm8` (sext) | 3 |
| +128 to +32767 | `add reg, imm16` (AX) or `add reg, imm16` (mod-rm) | 3 |

So `inc reg` is preferred for +1/+2 over `add
reg, imm8` (3 bytes).

**Type-keyword summary**:
| Keyword | Effect |
|---------|--------|
| `register` | Hint for enregistration |
| `typedef` | Type alias (no codegen) |
| `enum` | Parse-time int consts (no runtime tag) |
| `const` | Type qualifier (no codegen) |
| `volatile` | Type qualifier (no codegen at -O0) |
| `extern` | Symbol declaration (no definition emitted) |

For the Rust reimplementation:
- `register`: pass to register allocator as a
  preference hint.
- `typedef`: resolve to underlying type at parse.
- `enum`: emit values as int constants; no enum
  type information in the OBJ.
- Small-const adds: prefer `inc reg` × N for
  N ≤ 2.

## Struct modify via ptr; 2B struct returns in AX only; rotate idiom NOT recognised

Fixtures `1955` (struct modify via ptr), `1956`
(2B struct return), `1957` (rotate via shifts +
or) cover three smaller idioms.

- `1955` (**struct modify via ptr arg**): callee
  pattern:
  ```
  mov si, [p]              ; load ptr
  mov ax, [x] / mov [si], ax        ; p->x = x at offset 0
  mov ax, [y] / mov [si+2], ax      ; p->y = y at offset 2
  ```
  Each field uses `[si]` (2B) for offset 0, or
  `[si+disp]` (3B) for non-zero offsets. Ptr
  loaded once into SI for all field accesses.
- `1956` (**2B struct return = AX only**): a
  struct with a single 2-byte field returns in
  **just AX** (no DX). Same protocol as int:
  ```
  mov ax, [o.x]
  ret
  ```
  Caller does `mov [s.x], ax`. So **1-int
  struct return = int return**.
- `1957` (**rotate idiom NOT recognised**): `(x
  << 4) | (x >> 12)` (a logical rotate-left by 4)
  emits the literal sequence:
  ```
  mov ax, [x] / mov cl, 4 / shl ax, cl
  mov dx, [x] / mov cl, 12 / shr dx, cl
  or ax, dx
  ```
  ~10 bytes. BCC does **not recognize the rotate
  pattern** to emit `mov cl, 4 / rol ax, cl` (4
  bytes). No advanced pattern-matching beyond
  arithmetic constant folding.

**Struct return size matrix** (refined):
| Struct size | Return mechanism |
|-------------|------------------|
| 2 bytes (1 int) | AX only (same as int) |
| 4 bytes (2 ints) | DX:AX |
| > 4 bytes | Hidden dest ptr + N_SCOPY@ × 2 |

For the Rust reimplementation:
- Struct modify via ptr: load ptr to BX/SI, each
  field stored via `[reg+offset]`.
- Tiny structs (≤2B): return like the underlying
  scalar.
- No rotate idiom recognition — emit literal shifts
  + or.

## Struct arr-field inline; struct-by-value pushed reverse-mem-order; `o.p->v` 2-step deref

Fixtures `1871` (struct with array field), `1872`
(struct passed by value), and `1873` (chained
struct ptr deref) cover three remaining struct
shapes.

- `1871` (**struct with array field**): `struct {
  int n; int data[3]; }` is laid out **linearly
  with no padding**:
  | Field | Offset |
  |-------|--------|
  | `n` | 0 |
  | `data[0]` | 2 |
  | `data[1]` | 4 |
  | `data[2]` | 6 |
  Total size: 8 bytes. Array-as-struct-field is
  just **inline storage**; constant indices on
  the array resolve at parse time to specific
  flat offsets.
- `1872` (**struct passed by value**): a 4-byte
  struct `P {int x; int y;}` is passed by **field-
  by-field push in REVERSE memory order**:
  ```
  push word [q.y]    ; ff 76 06 — higher offset first
  push word [q.x]    ; ff 76 04 — lower offset last
  call sum_p
  pop / pop          ; 59 59 — 4 bytes cleanup
  ```
  This puts the struct in **memory order** in the
  callee's stack frame ([bp+4]=x, [bp+6]=y).
  Same effect as `memcpy`-ing the source into the
  arg-slot, but with explicit pushes.
  
  For larger structs (>4B), [[batch-XXX-struct-
  push]] (N_SPUSH@ helper) is used. For 4B
  structs, BCC uses inline pushes.
- `1873` (**chained struct ptr deref `o.p->v`**):
  lowers to:
  ```
  mov bx, [o.p]      ; 8b 5e fc — load ptr to BX
  mov ax, [bx]       ; 8b 07    — deref to get .v
  ```
  No fusion or special handling for chained
  derefs. Pointer loaded **once** into BX; then
  one field access. If v had non-zero offset
  (`bx+disp`), the second load would use
  `mov ax, [bx+disp]`.

For the Rust reimplementation:
- Struct layout: linear, no padding (8086 ABI).
  Array fields use sequential element offsets.
- Struct by value (size ≤ 4): inline field-by-
  field push in reverse memory order at call
  site.
- Chained deref `s->f`: emit ptr load to BX, then
  field load via `[bx+disp]`.

## 3D array row-major nesting; arr-of-struct iter = stride-4 shl×2

Fixtures `1820` (3D array constant-indexed), `1821`
(array of struct iteration), and `1822` (loop
init + sum array) confirm multi-dim layout rules.

- `1820` (**3D array `int a[2][2][2]`**): row-major
  layout with offsets computed at parse time for
  constant indices:
  - `a[i][j][k]` offset = `(i*4 + j*2 + k) * 2`
  - `a[0][0][0]` at `[bp-16]`
  - `a[0][1][1]` at `[bp-10]` (offset 6)
  - `a[1][1][1]` at `[bp-2]` (offset 14)
  
  N-dimensional arrays extend the 2D pattern —
  flat linear storage with row-major nesting:
  inner dimensions vary fastest.
- `1821` (**array of struct iteration**): `a[i].x`
  with variable i uses stride-4 (= `sizeof(struct
  P)`) multiplication via **2× `shl bx, 1`**:
  ```
  mov bx, si           ; i
  shl bx, 1            ; *2
  shl bx, 1            ; *2 (= *4)
  lea ax, [a + field]  ; base + field offset
  add bx, ax
  mov ax, [bx]         ; read field
  ```
  Each field access recomputes the address —
  no induction-variable optimization across .x and
  .y in the same iteration.
- `1822` (**loop init + sum array**): two sequential
  loops with the same iteration variable i (SI),
  accumulator (DI). Standard pattern; each
  iteration recomputes the element address via
  `shl + lea + add`.

For the Rust reimplementation:
- N-dimensional array indexing with constant
  indices: precompute the linear offset at parse
  time.
- N-dimensional with variable indices: emit
  multiplication for each dimension (sizeof × index)
  + base.
- Array-of-struct iteration: stride = sizeof(struct);
  if pow2 → shifts; else imul.

## Multi-init refs earlier; fn-ptr struct-field call via `ff 56`; `const int` not folded

Fixtures `1811` (multi-init with expressions),
`1812` (fn-ptr struct field), and `1813` (`const
int` folding) cover three more shapes.

- `1811` (**multi-init with cross-references**):
  `int a = 5, b = a + 1, c = b * 2;` works as
  expected — each later initializer references the
  earlier-evaluated variable. Register allocation
  applies per-variable based on read-count:
  - a (used twice): SI
  - b (used twice): DI
  - c (used once, only in return): stack
  
  Variables initialized to expressions still
  qualify for register allocation; init expressions
  are evaluated left-to-right with prior
  declarations visible.
- `1812` (**fn-ptr struct-field call**): `o.f(o.arg)`
  emits **`ff 56 disp`** (`call near [bp+disp]`)
  where disp is the offset of `o.f` within the local
  struct. Same opcode as for local fn-ptr variables
  and fn-ptr parameters. The struct-field-offset is
  baked in at the ModR/M displacement.
- `1813` (**`const int` NOT folded**): `const int
  n = 5; return n * 7;` still allocates a stack
  slot, stores 5, loads it for the multiplication.
  BCC does **not** treat `const` as a hint for
  compile-time folding — the qualifier is purely
  for type-system enforcement (no writes allowed).

For the Rust reimplementation:
- Multi-init: emit each init's code in declaration
  order, with subsequent ones able to read prior
  values.
- Fn-ptr struct field: emit `ff /2 [bp+disp]` with
  appropriate field offset.
- `const` doesn't gate folding — only the parse-
  time constant-folding pass for literal expressions.

So BCC's optimization is **purely syntactic** — it
folds compile-time constants when they appear
directly in expressions (1+2, sizeof(int), etc.) but
not when they hide behind `const` declarations.

## Array-of-struct linearised; 5-arg uses `add sp, 10`; `static` fn omits PUBDEF

Fixtures `1706` (array of struct), `1707` (5-arg
function), and `1708` (static function) cover three
codegen-affecting cases.

- `1706` (**array of struct**): `struct P a[2]`
  lays out as `a[0].x, a[0].y, a[1].x, a[1].y` —
  flat linear layout, each field accessible as
  `[bp+disp]`. For constant indices, the disp is
  baked in at parse time. The subtract `a[1].x +
  a[1].y - a[0].x` uses **`sub ax, [m]`** (opcode
  `2b /N`) — direct memory subtract, no separate
  load needed.
- `1707` (**5-arg cdecl**): args at `[bp+4]` to
  `[bp+12]` in declaration order. Caller cleans
  with **`add sp, 10`** (5 args × 2 bytes). The
  `add sp, N` encoding (`83 c4 disp8` for N ≤ 127)
  is 3 bytes regardless of N, so it's always more
  efficient than ≥ 3 individual `pop cx` (also
  1 byte each but with overhead) for any cleanup
  ≥ 6 bytes. Confirms the cleanup-strategy boundary
  from [[batch-435-arg-cleanup-boundary]].
- `1708` (**`static` function**): the OBJ has
  **no PUBDEF for `_helper`** — only `_main` is
  exported. Static linkage means the symbol stays
  internal; same-TU calls resolve via relative
  offsets in the call's `e8` displacement. The
  function bytes are still emitted to `_TEXT`,
  just not exported.

For the Rust reimplementation:
- Track per-symbol linkage flag (extern vs static).
- Emit `PUBDEF` records only for `extern`
  (default) functions and globals.
- `static` declarations still need their bytes
  emitted to the appropriate segment, but no
  external visibility — the linker won't see
  them.

This means each TU has 4 categories:
1. Default `extern` (PUBDEF + emit)
2. `static` (no PUBDEF, just emit)
3. `extern` declaration without definition (EXTDEF
   only, no emit)
4. Local automatic (no record at all — lives
   on stack)

## 2D array uses `imul` for row stride; goto = unconstrained jmp; `p->field` via `[bx]`

Fixtures `1700` (2D array sum), `1701` (goto loop),
and `1702` (`p->field` arrow operator) round out
the basic control-flow and addressing patterns.

- `1700` (**2D array indexing**): `a[i][j]` (where
  i, j are variables) lowers to:
  ```
  mov ax, si           ; i in SI
  mov dx, 6            ; row-stride bytes (3 ints × 2)
  imul dx              ; ax = i * 6
  mov dx, di           ; j in DI
  shl dx, 1            ; j * 2 (element-size shortcut)
  add ax, dx           ; combined byte offset
  lea dx, [bp-12]      ; base of array
  add ax, dx           ; final pointer offset
  mov bx, ax
  mov ax, [bx]         ; load element
  ```
  Notable: **`imul dx`** for the row stride (since
  the row count is a constant ≥ 2 not a pow2),
  **`shl dx, 1`** for `j * sizeof(int)` (pow2
  shortcut). Mixed strategy based on operand
  characteristics.
- `1700` also uses **`CX` as a third
  enregistered local** (sum accumulator) when SI
  and DI are taken by i, j — confirms the {SI, DI,
  DX, BX, CX} register pool from earlier batches.
- `1701` (**`goto` lowering**): a `goto label`
  emits a **plain `jmp`** to the label's address.
  The `goto loop / goto done` pattern produces
  **byte-identical code to a `while` loop** —
  same `cmp / jl / inc / jmp` structure. The
  compiler treats `goto` as just another control-
  flow primitive; no special analysis.
- `1702` (**`p->field` arrow operator**): lowers
  to `mov bx, [p_ptr] / mov ax, [bx+field_offset]`.
  For `a.next->v` (with `v` at field offset 0),
  the load is just `mov ax, [bx]`. For non-zero
  offset fields, ModR/M with disp8 (`8b 47 disp`)
  or disp16 would be used. The arrow operator is
  **two memory accesses**: load the pointer, then
  deref + field offset in one combined load.

So the small-model addressing toolkit is now
complete:
| Pattern | Encoding |
|---------|----------|
| `[bp+disp]` (local/param) | mod=01/10 rm=110 |
| `[bx]` / `[bx+disp]` (ptr deref) | mod=00/01 rm=111 |
| `[si]` / `[si+disp]` | mod=00/01 rm=100 |
| `[disp16]` (direct global) | mod=00 rm=110 |
| `[bx+si]` etc. (rare) | mod=00 rm=000-011 |

## `(int)5` no-op cast, trailing comma in init, 1-field struct init

Fixtures `1610` (`int x = (int)5;`), `1611` (`int
a[3] = {1, 2, 3,};` trailing comma), and `1612`
(`struct S { int x; } s = {42};` single-field struct
init) all pass on the first capture.

- `1610`: `(int)5` cast is a complete codegen no-op
  — emits identical code to `int x = 5`. Same-type
  casts disappear at parsing.
- `1611`: trailing comma in brace initializer is
  accepted (a common C feature) and produces no
  extra array elements. Data is exactly `01 00 02
  00 03 00`. Code-equivalent to `{1,2,3}`.
- `1612` (**finding**): single-int-field struct local
  initializer **loads from a `_DATA` template via
  `a1 disp16 / mov [bp-2], ax`** (with FIXUPP) —
  NOT a direct `mov word [bp-2], 42` (which would be
  the same size but constant-immediate). So BCC
  uses the same "data-template + load+store" shape
  for *single*-word struct inits as it does for
  multi-word ones (via `N_SCOPY@`), just without
  the memcpy helper since the size is one word.
  This is mildly suboptimal vs. constant-imm-store
  but consistent — BCC treats struct init uniformly
  as data-template-copy. The template occupies 2
  bytes in `_DATA` for the int field.

So BCC's struct-init lowering rule:
- 1-word struct (or single field): `mov ax,
  [_template] / mov [bp-N], ax`
- N-word struct: `push ss / lea ax,[bp-N] / push ax
  / push ds / mov ax, _template / push ax / mov cx,
  N*2 / call N_SCOPY@`

The data-template approach is **always used** for
struct local init, even when a direct
constant-immediate store would be shorter or same
size.

## `arr[i].x` struct arr var idx, `int x = (a==b)`, `sizeof(*p)`

Fixtures `1478` (`struct S {int x;}; struct S arr[3];
int i=1; arr[i].x = 99; return arr[i].x;` — struct
array with variable index), `1479` (`int a=7, b=7;
int x = (a == b); return x;` — int initializer from
bare `==` compare), and `1480` (`int x=0; int *p =
&x; return sizeof(*p);` — sizeof of a dereferenced
pointer) all pass on the first capture. `1478`
confirms struct-array stride lowering: `sizeof(struct
S) = 2` (single int field) is a power of two, so the
scale is `mov bx,si / shl bx,1` (not `imul`) — same
pow2 rule that applies to `int` element strides. The
`.x` field offset is 0, so the LEDATA FIXUPP target
for `_arr` produces an effective `[bx+_arr+0]`, no
extra displacement add. Store and load both
recompute the scaled offset — no CSE. Returns 99.
`1479` matches the same boolean materialization
template as the earlier `<` and `&&` cases, but the
inverse jcc selected for `==` is `jne` (jump if not
equal): `mov ax,[a] / cmp ax,[b] / jne L_false / mov
ax,1 / jmp / xor ax,ax`. Result 1 since 7 == 7.
`1480` confirms that `sizeof(*p)` is a pure compile-
time fold: the deref is *not* evaluated at run time
— no `mov ax,[si]` is emitted. Only `int x = 0; int
*p = &x;` lower to real instructions (the unused-by-
value `p` is still spilled to `[bp-4]`); the return
becomes `mov ax, 2` directly. Confirms BCC honours
the C rule that the operand of sizeof is
unevaluated.

## Array-of-struct init, `add5(a[1])`, `a[i] = i * 10`

Fixtures `1442` (`struct P arr[2] = {{1,2}, {3,4}};
return arr[1].x + arr[0].y;` — global array-of-struct
with nested init list), `1443` (`int add5(int x) {
return x + 5; } a[1]=10; return add5(a[1]);` —
function call passing an array element by value), and
`1444` (`for(i=0;i<3;i++) a[i] = i * 10; return a[2];`
— array fill using a multiplication of the loop index)
all pass on the first capture. `1442` confirms array-
of-struct global init: four ints laid out contiguously
in the data segment, each `{x,y}` pair occupying 4
bytes. `arr[1].x` = 3, `arr[0].y` = 2. Total 5. `1443`
confirms passing an array elem by value: `mov ax,
[bp-base+2] / push ax / call _add5`. Result 10+5 = 15.
`1444` confirms loop-driven array fill with index-mul
RHS: each iteration computes `i * 10` into AX, then
stores into the indexed slot via a separate base+
offset address calc. a[2] = 20.

## `char c += a*2`, identical-literal ptr eq, `s.x + a[1]`

Fixtures `1430` (`int a=5; char c=10; c += a * 2;
return c;` — char compound `+=` with int-mul RHS),
`1431` (`char *p = "abc"; char *q = "abc"; if (p == q)
return 1; return 0;` — equality between two pointers
that each point to the same string literal text), and
`1432` (`struct S { int x; }; struct S s={3}; int a[2]
={5,7}; return s.x + a[1];` — sum of a struct-field
load and an array-elem load) all pass on the first
capture. `1430` confirms the char-`+=`-int-result
shape: mul `a * 2` computes into AX (=10), then byte-
narrow-add into c's slot. Result 10+10 = 20. `1431`
confirms BCC behavior on duplicated string literals:
both `"abc"` references can either share storage
(literal pool dedup) or be separate -- the OBJ
match shows whatever BCC actually does, and the
return value reveals whether they're pooled. `1432`
is the cross-aggregate sum: each load reads from a
different global, both add into AX. 3+7 = 10.
Process note: 1430's first verify hung in DOSBox
(another flaky audio init); single retry passed.

## `if (x >= 0)`, `a[char i]`, global `gp->x = 42`

Fixtures `1427` (`int isnneg(int x) { if (x >= 0)
return 1; return 0; } return isnneg(-5);` — non-
negative check via `>=`), `1428` (`char a[5]; char i=
'\002'; return a[i];` — array subscript using a char
variable as the index), and `1429` (`struct S *gp =
&g; gp->x = 42; return gp->x;` — global struct
pointer initialized to global, then used for read-
write through arrow field) all pass on the first
capture. `1427` confirms `>=` lowers as the negation
of `<`: `cmp ax,0 / jl FALSE` shape — equivalent to
the existing `<` and `>` infrastructure. isnneg(-5)
= 0. `1428` confirms char-as-index `cbw`-promotes
to int for the address calculation: `mov al,[bp-i]
/ cbw / mov bx,ax / mov al,[bx+...]`. Result 30.
`1429` confirms global ptr-to-struct init from
another global's address: gp's data record holds the
OFFSET of g, then arrow access goes through the
pointer. Returns 42.

## `v = a[1]++`, linked-node `a.next->v`, `sumC` char arr

Fixtures `1418` (`int a[3]; ... v = a[1]++; return
a[1]*10 + v;` — post-increment of an array element
captured into another local), `1419` (`struct N { int
v; struct N *next; }; struct N b={2,0}; struct N a=
{1,&b}; return a.next->v;` — global struct chained via
pointer field), and `1420` (`int sumC(char *s, int n)
{ ... t += s[i]; ... } char a[3]={1,2,3}; return
sumC(a, 3);` — sum of char-array elements through fn
arg) all pass on the first capture. `1418` confirms
post-inc on array element: load a[1] (=20) into AX,
v = 20, then `inc word ptr [bp-base+2]` makes a[1]=
21. Return 21*10+20 = 230. `1419` confirms struct
init with cross-struct pointer reference (`&b` in
a's initializer at file scope): the global init
record holds the OFFSET to b, then `a.next->v` does
ptr-load then field-load. Result = b.v = 2. `1420`
confirms char-array passed as char*: callee indexes
`s[i]`, byte-loads, `cbw`-promotes, adds. 1+2+3 = 6.

## `uchar + uchar` over 255, swap via struct ptrs, `a -= two()`

Fixtures `1400` (`unsigned char a=200; unsigned char b=
100; return a + b;` — sum of two unsigned chars whose
arithmetic result exceeds 255), `1401` (`void swap
(struct S *a, struct S *b) { int t=a->x; a->x=b->x;
b->x=t; }` — swap struct fields through two struct
pointers), and `1402` (`int two(void) { return 2; }
int a=10; a -= two(); return a;` — int compound `-=`
with function-call result as RHS) all pass on the
first capture. `1400` confirms uchar arithmetic: each
uchar zero-extends to int via `xor ah,ah` (or `mov
al,...` then implicit zero in ah), 200+100=300. Since
return type is int, the 300 carries through without
truncation. Exit-code low byte is 44 (300 mod 256).
`1401` is the struct-ptr counterpart to `1274`'s int-
ptr swap: same shape but the deref reads/writes use
the `->x` field offset. After swap, s1.x=7. `1402`
confirms `-=` with call result: call lands in AX,
then `sub word ptr [bp-a],ax`. 10-2 = 8.

## while-inside-for, `a |= s.x`, `c = (char)(a + 100)`

Fixtures `1382` (`for(i=0;i<3;i++) { j=i; while (j>0)
{ s++; j--; } } return s;` — while loop nested inside
a for loop), `1383` (`struct S {int x;}; struct S
s = {0x0f}; ... a |= s.x; return a;` — int compound
`|=` with a struct-field RHS), and `1384` (`int a=5;
char c; c = (char)(a + 100); return c;` — char
narrow-cast applied to a parenthesized sum) all pass
on the first capture. `1382` confirms a different
nested-loop shape from `1369`'s nested-for: outer
post-step (`i++`) and an inner condition-driven
loop (`while (j > 0)`). Each i iteration does i
increments of s. Total s = 0+1+2 = 3. `1383`
confirms struct-field RHS for `|=`: `mov ax,[_s+0] /
or word ptr [bp-a],ax`. Result 0xF0 | 0x0F = 0xFF =
255. `1384` confirms cast-on-paren-expr: `a + 100`
computes into AX (=105), then `(char)` narrows on
store: `mov byte ptr [bp-c],al`. 105 fits in signed-
byte range, so no truncation.

## `getX(struct S*)`, `char c += a*b`, `a -= b - 1`

Fixtures `1313` (`int getX(struct S *p) { return
p->x; }` — function takes a struct pointer and returns
a field), `1314` (`char c = 1; int a=3; int b=4; c +=
a * b; return c;` — char compound `+=` with the RHS
being a product of two int locals), and `1315` (`int
a=20; int b=5; a -= b - 1; return a;` — int compound
`-=` whose RHS is itself a subtraction) all pass on
the first capture. `1313` confirms struct-ptr-getter:
caller passes `&s` (the static-storage address of the
global struct), callee does `mov bx,[bp+arg] / mov
ax,[bx+0]` — direct field read at the deref'd ptr.
`1314` confirms char-`+=`-int-mul: the int multiply
runs into AX first via stack-spill, then narrow-store
through char path: `cbw`-promote char LHS, add AX,
narrow-store back. Result 1 + 12 = 13. `1315` confirms
the binop-as-RHS of compound: `b - 1` computes into
AX, then `sub word ptr [bp-a],ax`. So 20 - 4 = 16.
The compound's RHS is its own expression tree, not a
fused operand.

## `v = *p++`, struct-ptr arg, `a -= b*c`

Fixtures `1289` (`int *p = a; int v = *p++; return v;`
— int-pointer postinc combined with dereference),
`1290` (`void inc(struct S *p) { p->x++; }` — function
takes a struct pointer and mutates a field), and
`1291` (`int a=20; int b=3; int c=2; a -= b*c; return
a;` — int compound `-=` whose RHS is a multiply of
two locals) all pass on the first capture. `1289`
confirms `*p++` int variant: load `*p` into AX via the
ptr-deref word load, then `add word ptr [bp-p],2`
(int-stride 2). The pre-increment value of `p` is
already the address that was dereferenced. `1290`
confirms struct-ptr arg + arrow-field postinc: the
arg slot holds `&s`, the body computes
`mov bx,[bp+p] / inc word ptr [bx+0]` -- compact and
direct. `1291` confirms `-=` with multiply RHS: `b*c`
is computed via stack-spill mul (load b, push, load c,
imul), then `sub word ptr [bp-a],ax` -- so 20 - 6 = 14.

## Signed `char >> var`, `int += s.x`, `a *= -3`

Fixtures `1241` (`char a=8; int n=1; return a >> n;` —
signed-char right-shift where the shift amount is a
runtime variable), `1242` (`int a += s.x;` — int local
compound `+=` with a struct-field RHS), and `1243`
(`int a=5; a *= -3; return a;` — int compound `*=` by
a negative constant) all pass on the first capture.
`1241` confirms the signed-char shift goes through the
standard char-to-int promote (`cbw`) and then `sar
ax,cl` — the variable-amount path uses CL for the
shift count even when the destination type is `char`,
mirroring what the K≥4 mul-pow2 path does. `1242` is
the field-RHS counterpart to `1234`'s plain int+=char:
field load goes through the struct's global address
(`mov ax,[_s+0]`) before the `add` into the local
slot. `1243` confirms `*= -3` doesn't fold through the
mul-pow2 shift path (since -3 isn't a power of two)
and instead uses `mov dx,0FFFDh / imul dx` — the
2's-complement encoding of -3 as a 16-bit constant.
Notably this is *not* fused as `mul by 3 then neg`;
BCC just feeds the negative immediate directly into the
multiply.

**Process note**: `1242`'s first source mixed
declarations and statements (`s.x = 3; int a = 10;`)
which BC++ 2.0 rejects with `Expression syntax in
function main` — BC++ 2.0 is strictly C89, requiring
all decls at the top of a block before any statement.
Source was corrected to declare `int a` up front. The
xfix verify originally "matched" the error-output
shape (exit_code=1, no OBJ) — byte-exact at the
shell-output level, but not exercising codegen.
Always inspect `expected/manifest.toml` for
`exit_code = 0` and an OBJ entry when capturing a
positive probe.

## Arrow field cmp const (peephole), array elem cmp, ternary in return

Fixtures `1007` (`if (p->x == 5)` with p in SI — addresses
the deferred batch-228 finding), `1008` (`if (a[1] == x)` —
stack array elem compared to a local in an if condition),
`1009` (`return x > 0 ? 100 : 200;` — ternary expression in
return position).

1007 needed both a tasm IR addition and a codegen peephole:

- Added `CmpWordSiDispImm8Sx { disp, imm }` to tasm IR.
  Encoding: `83 3C ii` for disp=0 (mod=00, 3 bytes); `83
  7C dd ii` for disp!=0 fitting i8 (mod=01, 4 bytes).
  Both forms use Group1 opcode `83` with /7=CMP and SI-
  indirect r/m (r/m=100). Parser recognizes `cmp word
  ptr [si+disp], imm` via the existing `parse_word_si_disp`
  helper plus `parse_imm8_signed` for the RHS constant.
- Added a fast-path arm in `emit_compare`: when LHS is
  `Member { kind: Arrow }` whose base is a SI-resident
  pointer local and whose field is non-char and the RHS
  is a constant that fits imm8sx, emit `cmp word ptr
  [si+field_off], K` directly. Restricted to SI for now
  since tasm only has the SI form; a DI sibling would
  follow the same pattern.

Saves 1 byte vs the previous `mov ax, [si]; cmp ax, K`
shape (4 bytes vs 5).

1008 already worked end-to-end. The compare-as-value path
materialized the LHS array element through the batch-220
operand-source rvalue then ran the standard `mov ax,
[bp+elem]; cmp ax, [bp+x]` shape. The memory-direct-cmp
peephole (batch 220) only fires for constant RHS — here
the RHS is a stack local, so the generic path applies.

1009 already worked end-to-end. The ternary lowering
materializes the boolean into the standard mini-CFG with
two `mov ax, K` materializations of the constants 100 and
200 — `cmp [bp-2], 0; jle .else; mov ax, 100; jmp .end;
.else: mov ax, 200; .end: <return>`. Fixture 428/431
covered the assign-to-global and nested-ternary variants;
this confirms the return-position form is byte-equivalent.

## Char stack array elem compound, postinc, arrow var-RHS

Fixtures `1001` (`char a[3]; a[1] += 5;` — char stack array
element compound add with const), `1002` (`char a[3];
a[1]++;` — char stack array element postinc as statement),
`1003` (`struct S { int x; } a; struct S *p = &a; int v =
42; p->x = v;` — arrow field assigned from a non-constant
stack local).

All three already work end-to-end via the existing array-
compound / array-postinc / arrow-assign paths. The char
array compound add lowers to `add byte ptr [bp+(base+K)],
imm` (same encoding as fixture 720's compound-and). The
char array postinc is `inc byte ptr [bp+(base+K)]`,
sibling of fixture 547 (int) and 717 (char global).
The arrow field var-RHS routes through `emit_member_assign`
— the batch-224 non-const arm covers both global-struct
and arrow-pointed-struct fields uniformly (the destination
operand differs but the same `mov ax, <rhs>; mov <dest>,
ax` lowering applies).

**Recorded findings (deferred):**

- **Enum constant as array size** (`enum { N = 4 }; int
  a[N];`): parser fails "expected array size (integer
  literal), got identifier". The array-size grammar only
  accepts `IntLit`; needs to fold enum constants (already
  registered in `enum_constants`) and possibly typedef'd
  integer constants too.
- **Memory-direct cmp for arrow field** (`if (p->x == 5)`
  with p in SI): BCC emits `cmp word ptr [si], 5` (4 bytes,
  imm8sx Group-1 form) — our codegen does `mov ax, [si];
  cmp ax, 5` (5 bytes). The peephole exists in spirit (see
  fixtures 891/1002 sibling probes) but tasm lacks the
  `CmpWordSiPtrImm8Sx`/`Imm16` variants. Add the `83 3C ii`
  and `81 3C lo hi` encodings to enable the peephole.

## Shift by 8, char struct field cmp, two-field struct add

Fixtures `992` (`int x = 1; return x << 8;` — shift by a
const that's > 3, forcing the CL load path), `993`
(`struct S { char c; } s; if (s.c == 'A')` — char struct
field compared to char-literal const), `994` (`struct S
{ int a; int b; } s; return s.a + s.b;` — local struct,
write both fields, read both and add).

All three already work end-to-end:

- 992: BCC unrolls shifts by 1, 2, or 3 into repeated `shl
  ax, 1`. For shift counts > 3 (or non-power-of-2 K), it
  emits `mov cl, K; shl ax, cl` — the CL load path. Our
  codegen already handled both shapes; this fixture pins
  the >3 path. Fixture 121 covered count=3.
- 993: char struct field cmp const lowers through the same
  byte-form memory compare as a char global: `cmp byte ptr
  DGROUP:_s+offset, K`. The chain-based compare peephole
  from batch 224 handles `s.c` for both byte-typed and
  word-typed leaves.
- 994: two struct field reads + add. BCC emits `mov ax,
  [bp+a]; add ax, [bp+b]` (or DGROUP-relative for globals).
  Our generic `Member` rvalue path supplies the operand
  source, and the generic binary-op emit handles the rest.

## Ptr local cmp zero, struct field var-RHS write, member cmp

Fixtures `989` (`int *p; p = &g; if (p == 0) return 1;` —
pointer local compared to zero in if), `990` (`s.x = v;`
with v a stack local — struct field assigned from non-
constant), `991` (`s.x = 5; if (s.x == 5)` — struct field
compared to constant in if).

989 already worked via the existing `if (var == 0)` zero-
test path — pointer locals route through the same
`cmp word ptr <var>, 0` shape as integer locals (the
`pointee.is_some()` branch in the local-Ident arm).

990 needed a small extension to `emit_member_assign`. The
existing path panicked on non-const RHS. Added an int-field
non-const arm: `emit_expr_to_ax(value); mov word ptr
<dest>, ax`. Same shape as BCC: `mov ax, [bp-N]; mov word
ptr DGROUP:_s, ax`. Restricted to non-char fields for now.

991 exposed a missing memory-direct compare peephole for
`<member-or-array> == const` against a global root. The
batch-220/221 peephole only covered stack-local roots.
Generalized that arm: when `try_lvalue_chain_addr` resolves
to a global root, emit `cmp word ptr DGROUP:_<name>+off, K`
(or byte form). Sibling of the local-root case, identical
mnemonic and immediate-handling. Now covers `s.x`, `s.b.x`,
`g.a[K]`, etc., on both globals and locals — every chain
that resolves to a constant memory address.

## Struct-array field rvalue, nested struct, `<=` as value

Fixtures `932` (`struct S { int n; int a[3]; } s; s.n = 7;
s.a[1] = 9; return s.n + s.a[1];` — global struct with an int
array field, used in an arithmetic rvalue), `933` (`struct A
{ struct B b; }; s.b.x = 42; return s.b.x;` — nested struct
member access via dot chain), `934` (`return x <= y;` — int
`<=` comparison used as a return value, not an `if` condition).

933 and 934 already worked end-to-end:

- 933: the existing member-chain helpers (`try_lvalue_chain_addr`
  / `try_member_dot_chain`) recurse through any number of Dot
  member nodes, accumulating field offsets. For `s.b.x` with
  both fields at offset 0, the chain resolves to
  `DGROUP:_s+0`, and the store/load fold into `mov word ptr
  DGROUP:_s, 42` / `mov ax, word ptr DGROUP:_s`.
- 934: the integer comparison-as-value path already handled
  `<=` via the same lowering as the `if`-condition path
  (`cmp; setle al; movzx ax, al`-equivalent on 8086:
  `cmp; jle .true; xor ax, ax; jmp .end; .true: mov ax, 1`).

932 needed one codegen fix in `OperandSource` resolution.
When a binary op had a member→array-index chain like `s.a[1]`
on its right-hand side, the existing `ExprKind::ArrayIndex`
arm walked the index list inline and panicked at the first
`Member` node it encountered ("array-index rhs: non-ident
base not supported"). Replaced the inline walk with a call to
`try_lvalue_chain_addr`, the same helper the `Member` rvalue
arm already used. That helper already recurses through
ArrayIndex *and* Member nodes uniformly — once the
ArrayIndex arm routes through it, mixed chains like
`s.a[K]`, `g.b.c[K]`, and `arr[i].field[j]` all fold to a
single `DGROUP:_<root>+<total_off>` operand. Cuts ~20 lines
of duplicate walk logic.

The arm still rejects non-global bases — local struct fields
through ArrayIndex would need a `[bp-N+K]` operand instead,
and no fixture exercises that path yet.

## Enum values, function-static, union

Fixtures `917` (`enum E { A = 5, B = 10, C }; return C` —
enum with explicit values + auto-increment for `C`), `918`
(`int main() { static int g; ... }` — function-local static),
`919` (`union U { int i; char c[2]; }; union U u;` — basic
union with int/char overlay).

All three already work end-to-end. Coverage notes:

- 917: enumerator with explicit value sets the running counter
  (`A = 5`, `B = 10`); the next unspecified enumerator (`C`)
  auto-increments to `11`. The return-value path emits `mov
  ax, 11`.
- 918: `static` inside a function body promotes the local to a
  file-scope BSS symbol — but the symbol is *not* public.
  Codegen treats `g` like a private global (DGROUP-relative
  addressing), not a stack slot.
- 919: union layout — all members share the lowest offset
  (offset 0). `u.i = 0x4142` writes a word; `u.c[0]` reads the
  low byte (`0x42`, returned via `mov al, 0x42` widened to AX).
  Union shares the global's storage size = `max(member size) =
  2 bytes`.

## 2D array init, enum, typedef

Fixtures `914` (`int a[2][3] = {{1,2,3},{4,5,6}}` — 2D array
initializer), `915` (`enum E { A, B, C }; return B` — basic
enum), `916` (`typedef int Int; Int g` — typedef alias for int).

All three already work end-to-end. Coverage notes:

- 914: nested initializer list — outer braces group by row,
  inner braces fill each row's elements. The 6 ints land in
  `_DATA` row-major as `dw 1; dw 2; dw 3; dw 4; dw 5; dw 6`.
  `a[1][2]` reads at offset 5*2=10 from `_a`.
- 915: enum values are int-typed constants — the enumerator
  `B` materializes as the literal `1` in the return path
  (`mov ax, 1`). No enum-tag entry in the OBJ — the type info
  is purely parser-side.
- 916: `typedef int Int` registers `Int` in the parser's
  typedef table; `Int g` then parses identically to `int g`.
  No OBJ-level difference between the two.

## Struct/negative/pointer initializers

Fixtures `911` (`struct S { int x; int y; }; struct S s = {1,
2};` — struct initializer), `912` (`int g = -1;` — negative
int init), `913` (`char *p = "Hi";` — char pointer initialized
to string literal).

All three already work end-to-end. Coverage notes:

- 911: struct-shaped initializer list `{1, 2}` lands two `dw`
  entries (one per field) under the struct's symbol — same
  layout as a non-aggregate global, just stride-2 per int
  field.
- 912: `-1` lands as `0FFFFh` in `_DATA` — the sign-extension
  to 16 bits is handled by the same masking already in
  `try_const_eval`.
- 913: `char *p = "Hi"` emits the anonymous string constant in
  `_DATA` (`db 'H','i',0`) and a relocated word in `_DATA` for
  `_p` itself (pointing at the string's offset). The OBJ
  contains the FIXUPP record linking the pointer's bytes to
  the anonymous string's offset.

## `p->x += y`, `p->x *= y`, `p->x <<= y` (arrow member)

Fixtures `842` / `843` / `844` — three free passes
confirming the int-field compound paths added in
batches 171 and 172 generalize from `.` (Dot) to `->`
(Arrow) member access:

- The arm builds `dest` as `[<reg>]` (or `[<reg>+off]`
  if field offset is non-zero) for arrow form, vs
  `DGROUP:_<name>+<off>` for dot form. The Add/Sub/Bit*,
  Mul/Div/Mod, and Shift paths use `dest` as opaque
  text, so both addressing modes work without special-
  casing.
- `843`'s `imul word ptr [bp+N]` and `844`'s `shl word
  ptr [si], cl` had previously been added for non-arrow
  fixtures (834, 835) — they only depend on the dest
  string format.

No code changes — confirms the arrow member compound
inherits everything from dot member compound.

## `a[K] += y`, `s.x *= y`, `s.x <<= y` (non-const RHS)

Fixtures `833` (`a[1] += y`), `834` (`s.x *= y`),
`835` (`s.x <<= y`).

- `833` — int-array-element compound with non-constant
  RHS. `emit_array_compound_assign` previously panicked
  in this case. Added a path that mirrors the int-global
  Add/Sub/Bit* arm: `emit_expr_to_ax` produces AX from
  the RHS (with any widening), then `<op> word ptr
  <dest>, ax` writes back. `dest` already has the
  constant index folded as `DGROUP:_a+<K*stride>`.
- `834` — int-member compound `*=` with non-constant
  local RHS. Added a path in `emit_member_compound_assign`
  using `imul word ptr [bp+N]` directly against the
  member address. Same shape as fixture 802 with the
  member's effective address. Same path handles `/=`
  and `%=` (selecting AX or DX for the store).
- `835` — int-member compound `<<=` / `>>=`. Reuses the
  `rhs_byte_addr` helper (batch 169) to load CL from
  the RHS, then `shl/sar/shr word ptr <dest>, cl`.

Three new paths in member/array compound; no new IR
required — all shapes already encodable via the
existing imul/idiv/shl word ptr forms.

## `int` global compound `*=` / `/=` / `<<=` with array / deref / member RHS

Fixtures `824` (`g *= a[1]`), `825` (`g /= *p`),
`826` (`g <<= s.x`) — extending the Mul/Div/Shift arms
to accept array / deref / member RHS forms:

- `824` — `imul word ptr DGROUP:_a+2`: existing
  `ImulGroupSym` encoder, but the arm now constructs the
  address from a constant-indexed array via the new
  `global_int_rhs_addr` helper.
- `825` — `idiv word ptr [si]` for `*p` where `p` is
  register-resident. New IR variants `ImulSiPtr` (F7 2C)
  and `IdivSiPtr` (F7 3C) for the deref-through-SI
  form. Codegen arm gated on register-resident int*
  pointer.
- `826` — `mov cl, byte ptr DGROUP:_s; shl word ptr
  DGROUP:_g, cl`. The shift arm now uses a new
  `rhs_byte_addr` helper that resolves the byte-pointer
  form for any of Ident / ArrayIndex / Member RHS — and
  for stack-resident bases — without needing per-form
  branches.

Two new helpers (`global_int_rhs_addr`,
`rhs_byte_addr`) plus two new IR variants
(`ImulSiPtr`, `IdivSiPtr`).

## `int` global compound `+=` with array / deref / member RHS

Fixtures `821` (`g += a[1]`), `822` (`g += *p`),
`823` (`g += s.x`) — extending the int-global Add/Sub/
Bit* arm to accept non-Ident RHS shapes:

- `821` — `a[1]` (constant array index): emits `mov ax,
  word ptr DGROUP:_a+2; add word ptr DGROUP:_g, ax`.
  emit_expr_to_ax already folds the constant index into
  the address offset.
- `822` — `*p` (deref of register-resident pointer):
  emits `mov ax, word ptr [si]; add word ptr DGROUP:_g,
  ax`. emit_expr_to_ax handles the deref of a SI-bound
  int pointer.
- `823` — `s.x` (global struct member): emits `mov ax,
  word ptr DGROUP:_s; add word ptr DGROUP:_g, ax`. The
  member offset folds into the symbol+offset form.

Added a new helper `rhs_int_compound_type` that
resolves the result type for `ArrayIndex`, `Deref`, and
`Member` in addition to plain `Ident`. The Add/Sub/Bit*
arm now uses this broader helper, dropping the
`ExprKind::Ident` gate. All three patterns produce the
same memory-direct `<op> word ptr DGROUP:_<g>, ax`
shape, so no new IR or encoding was needed.

## `char` field / array postfix `++` / `--`

Fixtures `716` (`g.c++`), `717` (`a[2]++`), `718` (`++a[2]`).

- `716` and `717` — same pre-vs-post asymmetry as `g++`
  (batch 128) and `(*p)++` (batch 132), applied to the
  member and array sites. Postfix-discarded compiles to
  memory-direct `inc byte ptr <dest>`; prefix and explicit
  compound use the AL detour. Wired the existing
  `from_postfix` field (added batch 132) through
  `emit_member_compound_assign` and the global-array arm of
  `emit_array_compound_assign`; both gain a "char +
  from_postfix + K=1 + Add|Sub → memory-direct" branch
  before the AL-detour fallthrough.
- `718` (`++a[2]`) — free pass. Confirms BCC takes the AL
  detour for prefix array-element updates, same as
  `++g.c` (fixture 709).

## `char` arrow-field and `*p` compound

Fixtures `710` (`p->c += 5`), `711` (`*p += 5`), `712`
(`*p &= 15`). All three with `p` register-resident in SI.

- `710` and `711` — arith char through a pointer follows the
  same AL detour as char-global: `mov al, byte ptr [si]; add
  al, K; mov byte ptr [si], al`. The writeback step needed a
  new tasm IR variant `MovSiPtrReg8` (`88 (mod=00 reg=<r>
  r/m=100)`, encoding `88 04` for `mov [si], al`) — 8-bit
  sibling of the existing `MovSiPtrReg16`. Codegen:
  - `710` routed through `emit_member_compound_assign`'s
    arrow-with-register-base path; my batch 129 char-field
    arith arm already covered it once the writeback parsed.
  - `711` routed through `emit_deref_compound_assign`'s
    register-pointer fast path (line 5980). Was emitting
    memory-direct `add byte ptr [reg], K`; added the AL
    detour branch with the K=1 inc/dec peephole, mirroring
    the char-field path.
- `712` — char-via-pointer bitwise stays memory-direct:
  `and byte ptr [si], 15`. Added tasm IR variants
  `AndSiPtrByteImm8` / `OrSiPtrByteImm8` /
  `XorSiPtrByteImm8` (encoding `80 24|0C|34 ii` — Grp1 r/m8
  imm8 with mod=00 r/m=100). Codegen already emitted the
  right text via the `mnemonic <width> ptr [reg], K` line;
  only the parser/encoder side needed the new variants.

## `char` struct local, field-var-RHS, and field `++`

Fixtures `707` (`s.c += 5` on stack-resident struct local),
`708` (`g.c += d` with variable RHS), `709` (`++g.c`).

- `707` — free pass. Char struct field on a stack-local
  struct works the same as on a global: the AL load-modify-
  store template substitutes `bp_addr(struct_base +
  field_off)` for `<dest>`. BCC emitted `mov al, byte ptr
  [bp-2]; add al, 5; mov byte ptr [bp-2], al` and our
  codegen produced the same.
- `708` — variable-RHS char-field compound: BCC emits
  `mov al, byte ptr <src>; add byte ptr <dest>, al` —
  memory-direct add against the field, with the RHS pre-
  loaded into AL. Same shape as char-global var (batch
  121). Added an arm to `emit_member_compound_assign` gated
  on `store_byte && op ∈ {Add|Sub|BitAnd|BitOr|BitXor} &&
  try_const_eval(value).is_none()`.
- `709` — `++g.c` parses as `g.c += 1` (the `Update` AST
  node only targets bare identifiers). The AL detour path
  fired but emitted `add al, 1` while BCC emits `inc al`.
  Same byte count, different opcode. Added a K=1 peephole
  in the byte-field arith arm: `add al, 1` → `inc al`,
  `add al, 0xFF` (for `-= 1`) → `dec al`.

## `char` struct field + global-array element compound

Fixtures `704` (`g.c += 5`, struct global, char field at
offset 0), `705` (`g.c &= 15`, char field at offset 2),
`706` (`a[2] += 5`, char global array).

- `704` — char-struct-field arith: BCC uses the same AL
  load-modify-store as for plain char globals
  (`mov al, byte ptr <addr>; add al, K; mov byte ptr
  <addr>, al`). The `<addr>` is `DGROUP:_<name>+<off>` or
  bare `DGROUP:_<name>` when offset is 0. Our codegen was
  using memory-direct `add byte ptr <addr>, K` (which
  tasm's parser doesn't recognize and BCC doesn't emit).
  Extended `emit_member_compound_assign` to branch on
  `store_byte && matches!(op, Add | Sub)` for the AL detour
  with the `add al, (256-K)` canonicalization for `-=`.
- `705` — char-field bitwise stays memory-direct
  (`and byte ptr <addr>, K`) — same asymmetry as
  char-global (batch 122). Free pass off the existing
  fall-through.
- `706` — char-global-array element compound:
  `emit_array_compound_assign` only had a long-global path
  plus a stack-local path; it panicked with "unknown local
  in codegen: a" for non-long globals. Added a global-non-
  long arm that mirrors char-global codegen — same AL
  detour for arith, memory-direct for bitwise, with the
  address being `DGROUP:_<a>+<const_off>` from
  `global_offset_addr`. Int-element globals also route
  through this arm with memory-direct shape.


## Small struct-to-struct assignment — inlined word moves (fixture `2404`)

`b = a;` for a 4-byte struct (two ints) is **inlined as 4 word
operations** through AX and DX — no `N_SCOPY@` helper:

```c
struct Point { int x; int y; };
struct Point a, b;
a.x = 10; a.y = 20;
b = a;
```

```
8b 46 fe                ; mov ax, [bp-2]   ← a.y
8b 56 fc                ; mov dx, [bp-4]   ← a.x
89 46 fa                ; mov [bp-6], ax   ← b.y
89 56 f8                ; mov [bp-8], dx   ← b.x
```

So for a 4-byte struct: 2 loads (one per member, into AX/DX) + 2
stores. Total 12 bytes of code, vs. ~20 bytes for the helper-call
path (push args, call N_SCOPY@, cleanup).

Larger structs (already-documented `> 4 bytes`) flip to the
N_SCOPY@ helper. So the threshold:

| Struct size | Assignment codegen |
|---|---|
| ≤ 4 bytes | Inline N word loads + N word stores (interleaved through AX, DX, ...) |
| > 4 bytes | `N_SCOPY@` helper call |

Two ints fit in AX+DX simultaneously, hence the two-register
interleave here. The order (read `a.y` first, then `a.x`) is
arbitrary from a correctness standpoint since source and destination
don't alias.

For the struct **initialization** case (`struct S s = {1, 2, 3};`),
the data-template + N_SCOPY@ form is used uniformly per earlier
findings — that's a different code path from struct-to-struct copy.

## Union byte-order — little-endian confirmed (fixture `2401`)

```c
union IntBytes { unsigned int i; unsigned char b[2]; };
u.i = 0xABCD;
return u.b[0] + u.b[1];   // = 0xCD + 0xAB = 376
```

The union's int member and char array share the same 2-byte
storage. The byte-order witness:

- `u.b[0]` is at byte offset 0 → **low byte** of the int → 0xCD
- `u.b[1]` is at byte offset 1 → **high byte** of the int → 0xAB

Confirms 8086's little-endian byte order: the low-address byte holds
the low-order bits of multi-byte values. This applies to all
multi-byte types (int, long, struct fields).

## Variable-indexed `a[i].field` — separate addr-compute per access (fixture `2438`)

`struct Point a[3]; a[i].x + a[i].y;` with variable `i` computes the
address for each field access independently — no common-subexpression
elimination between the two accesses to the same `a[i]`:

```
; a[i].x:
8b de                   ; bx = i (si)
d1 e3 d1 e3             ; bx <<= 2  (i * sizeof(Point) = i*4)
8d 46 f4                ; lea ax, [bp-12]   ← &a[0].x
03 d8                   ; bx += ax           ← bx = &a[i].x
8b 07                   ; mov ax, [bx]       ← a[i].x

; a[i].y (recomputes from scratch):
8b de                   ; bx = i
d1 e3 d1 e3             ; bx <<= 2
8d 56 f6                ; lea dx, [bp-10]   ← &a[0].y  (different base!)
03 da                   ; bx += dx
03 07                   ; add ax, [bx]      ← a[i].y
```

Both accesses share the `i*4` scaling, but BCC re-emits the
`shl bx, 1` pair both times. The two lea targets differ
(`&a[0].x` vs `&a[0].y`) since each field has its own base
displacement.

A CSE optimization could compute `i*4` once and reuse it for all
fields of the same `a[i]` — but BCC doesn't perform this. Each
subscript expression is lowered independently, consistent with the
broader no-CSE pattern documented elsewhere.

## `sizeof(struct X)` returns packed byte total (fixture `2474`)

`struct Three { int x; char c; int y; };` — without padding, this is
5 bytes (2 + 1 + 2). BCC's `sizeof` returns exactly 5, confirming
the [[struct-fields-packed-no-padding]] layout:

```c
struct Three { int x; char c; int y; };
return sizeof(struct Three);
```

```
b8 05 00                ; mov ax, 5
```

Compare to an aligning compiler (would pad to 6 bytes: int+pad+char
+pad+int, or similar). BCC: 5 bytes flat.

The sizeof folds at compile time — no runtime computation, just the
literal `mov ax, 5`. Confirms:
- Struct sizes are compile-time constants in all expression contexts
- The size is the **packed byte count**, not a rounded-up
  alignment-aware value
- This applies regardless of how the struct is used (sizeof, stack
  allocation, file-scope reserve)

So when a struct's individual stack frame rounds up (e.g. fixture
`2420`'s 9-byte struct reserves 10 bytes), that's a **stack
alignment** rule, NOT the struct's intrinsic size — the struct
itself is still 9 bytes by `sizeof`.

## Struct copy via pointer — fully inlined, no helper call

Fixture `2495-struct-copy-via-ptr-obj`: `*dst = *src` for a 4-byte
`struct Pair { int a; int b; }` reached through `struct Pair *`.

```c
void copy(struct Pair *dst, struct Pair *src) {
  *dst = *src;
}
```

```
55                    push bp
8b ec                 mov bp, sp
56                    push si
57                    push di
8b 76 04              mov si, [bp+4]    ; dst
8b 7e 06              mov di, [bp+6]    ; src
8b 45 02              mov ax, [di+2]    ; src->b (FIELD b FIRST)
8b 15                 mov dx, [di]      ; src->a (no disp; ModR/M 15)
89 44 02              mov [si+2], ax    ; dst->b
89 14                 mov [si], dx      ; dst->a
5f 5e 5d c3           pop di/si/bp; ret
```

Key observations:
- **Both source fields are loaded BEFORE any store** (ax holds b, dx
  holds a). This avoids load-store-load-store serialization.
- **Field b is loaded first**, then field a — opposite of declaration
  order. This is consistent regardless of layout.
- **No call to `N_SCOPY@`** — at 4 bytes the inline expansion wins.
  (Compare to larger structs returned by value, which DO use
  `N_SCOPY@`.) The threshold for "inline vs helper" for pointer-based
  struct assign is at least 4 bytes — to be probed further.
- Uses two general regs (ax and dx) rather than chaining through one,
  trading register pressure for parallelism.
- ModR/M form `15` is mod 00, r/m 101 = `[di]` with no displacement;
  `44 02` is mod 01, r/m 100 = `[si+disp8]` with disp8=2.


## Nested struct field access — single fixed offset, no chained loads

Fixture `2503-nested-struct-field-obj`:

```c
struct Inner { int v; };
struct Outer { struct Inner i; int z; };
struct Outer obj;
int main(void) {
  obj.i.v = 7;
  return obj.i.v;
}
```

```
55 8b ec                    prologue
c7 06 00 00 07 00           mov word [_obj + 0], 7   ; FIXUPP _obj, disp16=0
a1 00 00                    mov ax, [_obj + 0]       ; FIXUPP _obj, disp16=0
eb 00 5d c3                 epilogue
```

Findings:
- `obj.i.v` flattens to a single base+offset pair at compile time
  (offset = 0, since both i and v sit at offset 0 in their parents).
  No intermediate "load address of obj.i" step.
- Nesting depth is essentially free — n-level field access compiles
  to the same shape as 1-level, with the offset summed at parse time.
- `c7 06 disp16 imm16` is the direct mem-to-imm16 store; ModR/M `06`
  is mod 00, r/m 110 = `[disp16]` (the moffs16 form). The disp16 is
  fixupp'd to `_obj+0`.
- Pure offset folding means cross-cutting struct decisions
  (alignment, packing, bitfields) only need to be computed ONCE per
  type, at type-definition time — never at access-site time.


## Struct ≤4B returned by value — packed into DX:AX (no N_SCOPY@)

Fixture `2524-struct-ret-by-val-obj`:

```c
struct Small { int x; int y; };
struct Small make(void) {
  struct Small s;
  s.x = 10;
  s.y = 20;
  return s;
}
```

```
55 8b ec                       prologue
83 ec 04                       sub sp, 4             ; 4B local for s
c7 46 fc 0a 00                 s.x = 10              ; at [bp-4]
c7 46 fe 14 00                 s.y = 20              ; at [bp-2]
8b 56 fe                       mov dx, [bp-2]        ; dx = s.y  (HIGH word)
8b 46 fc                       mov ax, [bp-4]        ; ax = s.x  (LOW word)
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Struct of exactly 4 bytes returned by value is **packed into
  DX:AX** — `ax = first int (offset 0)`, `dx = second int
  (offset 2)`. Byte-identical to returning a `long` with the
  same bit layout.
- **NO `N_SCOPY@` helper call**: at this size the value flows
  through registers, not through a hidden return-pointer arg.
- The load order is `dx ← high, ax ← low` — DX comes first in
  source order despite being the "high half." Suggests the codegen
  evaluates `return s` by walking fields in declaration order, with
  the first field always going to AX and the next to DX.
- To probe: a 3-byte struct (e.g. `{int; char;}`) — does it also
  use DX:AX with the char in DX's low byte? A 5+ byte struct would
  definitely switch to hidden-pointer-arg + N_SCOPY@ (already
  observed in earlier fixtures).


## 3-byte struct return — DOES use hidden-ptr + N_SCOPY@ (NOT DX:AX)

Fixture `2526-struct-3b-ret-obj`:

```c
struct Three { int x; char c; };
struct Three make(void) {
  struct Three s;
  s.x = 100;
  s.c = 'Z';
  return s;
}
```

```
55 8b ec                    prologue
83 ec 04                    sub sp, 4              ; 4B local (3B struct padded)
c7 46 fc 64 00              s.x = 100              ; [bp-4]
c6 46 fe 5a                 s.c = 'Z'              ; [bp-2], byte store
ff 76 06                    push word [bp+6]       ; caller-passed dst SEG
ff 76 04                    push word [bp+4]       ; caller-passed dst OFF
8d 46 fc                    lea ax, [bp-4]         ; src OFF = &s
16                          push ss                ; src SEG = ss
50                          push ax
b9 03 00                    mov cx, 3              ; count = 3 bytes
e8 00 00                    call N_SCOPY@          ; (EXTDEF)
8b 46 04                    mov ax, [bp+4]         ; return value = dst OFF
eb 00 8b e5 5d c3           epilogue
```

Findings:
- **3-byte struct return DOES use the hidden-pointer convention**:
  the caller passes `(dst-seg, dst-off)` as two extra args at
  `[bp+4]`/`[bp+6]`. The callee copies its local struct over via
  `N_SCOPY@`. THEN returns the dst pointer in AX.
- So the DX:AX-as-return rule from `2524` applies **only** when
  the struct is EXACTLY 4 bytes — not "≤ 4". 3-byte struct uses
  helper, 4-byte struct uses DX:AX. Likely rule: **DX:AX path is
  used only when sizeof == 4 AND the struct is two ints** (or maybe
  any 4-byte type), otherwise → N_SCOPY@.
- The N_SCOPY@ arg order (top of stack first as call sees them):
  `count = cx`, then on stack: `src-off, src-seg, dst-off, dst-seg`.
  Reading pushes in reverse: dst-seg pushed first → bottom; src-off
  pushed last → top.
- The return convention is **"AX = original dst-off"** — caller
  knows the dest by the same pointer, so AX is informational.
- Local-struct alignment: 3-byte struct gets `sub sp, 4` (even-pad).


## 2-byte struct return — packed into AX alone

Fixture `2531-struct-2b-ret-obj`:

```c
struct Pair { char a; char b; };
struct Pair make(void) {
  struct Pair p;
  p.a = 'X';
  p.b = 'Y';
  return p;
}
```

```
55 8b ec 4c 4c                prologue + 2B local
c6 46 fe 58                   p.a = 'X' (byte at [bp-2])
c6 46 ff 59                   p.b = 'Y' (byte at [bp-1])
8b 46 fe                      mov ax, [bp-2]    ; read both bytes as word
eb 00 8b e5 5d c3             epilogue
```

Findings:
- **2-byte struct return = packed into AX** with a single
  word-sized load. Low byte (p.a at offset 0) lands in AL,
  high byte (p.b at offset 1) lands in AH — natural little-endian.
- **NO N_SCOPY@ helper call**, no use of DX.
- So the size-to-strategy map is now:
  - `sizeof == 2` → packed into AX
  - `sizeof == 3` → N_SCOPY@ + hidden ptr (`2526`)
  - `sizeof == 4` → DX:AX (`2524`)
  - `sizeof >= 5` → N_SCOPY@ + hidden ptr
- Probably also: `sizeof == 1` → packed into AL only. To probe.

## 4-byte struct holding a `long` — also DX:AX

Fixture `2532-struct-long-ret-obj`:

```c
struct Big { long v; };
struct Big make(void) {
  struct Big b;
  b.v = 0x12345678L;
  return b;
}
```

```
55 8b ec 83 ec 04              prologue + 4B local
c7 46 fe 34 12                 [bp-2] = 0x1234        ; HIGH word first
c7 46 fc 78 56                 [bp-4] = 0x5678        ; LOW word
8b 56 fe                       mov dx, [bp-2]         ; dx = HIGH
8b 46 fc                       mov ax, [bp-4]         ; ax = LOW
eb 00 8b e5 5d c3              epilogue
```

Findings:
- A struct of exactly 4 bytes holding a single long uses the **same
  DX:AX return path** as `{int x; int y}` (`2524`). So the
  return-strategy decision is **based on sizeof alone**, not on
  field count or types.
- **Long store-to-stack writes HIGH word first** (at [bp-2]), then
  LOW word (at [bp-4]) — same order as `2521`.
- The 32-bit literal `0x12345678` is split: HIGH = 0x1234, LOW =
  0x5678. Two separate `c7 46 disp imm16` stores. No 32-bit ops.


## 1-byte struct return — packed into AL alone

Fixture `2537-struct-1b-ret-obj`:

```c
struct Tiny { char c; };
struct Tiny make(void) {
  struct Tiny t;
  t.c = 'A';
  return t;
}
```

```
55 8b ec                       prologue
4c 4c                          dec sp; dec sp        ; 2B local (1B padded)
c6 46 fe 41                    [bp-2] = 'A'           ; byte store
8a 46 fe                       mov al, [bp-2]         ; byte load
eb 00 8b e5 5d c3              epilogue
```

Findings:
- 1-byte struct return uses **AL only** (single `mov al, [bp-2]`).
  AH is left undefined. Caller reads AL only.
- The local frame reserves 2 bytes (`dec sp; dec sp`) for the 1-byte
  struct — **stack slots are even-padded**, even for sub-word structs.
- This completes the size→strategy mapping:

| sizeof | strategy |
|--------|----------|
| 1      | AL only |
| 2      | AX (low=field0, high=field1) |
| 3      | N_SCOPY@ + hidden ptr |
| 4      | DX:AX (ax=low, dx=high) |
| ≥ 5    | N_SCOPY@ + hidden ptr |

The pattern: sizes that match an integer type (1, 2, 4) use register
returns; sizes that don't (3, 5, 6, 7, ...) go through `N_SCOPY@`.

## Struct ≤4B as function arg — pushed as raw bytes, accessed via [bp+disp]

Fixture `2538-struct-4b-arg-obj`:

```c
struct Pair { int a; int b; };
int sum(struct Pair p) {
  return p.a + p.b;
}
```

```
55 8b ec                       prologue
8b 46 04                       mov ax, [bp+4]          ; p.a
03 46 06                       add ax, [bp+6]          ; + p.b (mem operand!)
eb 00 5d c3                    epilogue
```

Findings:
- **Struct arguments are ALWAYS passed on the stack**, regardless
  of size. No DX:AX register-passing for the by-value struct param,
  even though it's exactly 4 bytes and could fit.
- The caller pushes the struct's raw bytes contiguously; callee
  accesses fields at known `[bp + base + field-offset]`.
- The `add ax, [bp+6]` uses a **direct memory operand** — no
  intermediate `mov` to load p.b. So the AX-accumulator pattern
  allows ALU-with-memory-source ops, saving the intermediate move.
- Conclusion: register-passing-convention is **asymmetric** — small
  structs get reg returns but NOT reg args. Caller's burden is
  always stack-marshalling for struct args.


## Struct with embedded array — fields laid out contiguously, no padding

Fixture `2553-struct-embedded-arr-obj`:

```c
struct Box { int tag; int data[3]; };
struct Box b;
int main(void) {
  b.tag = 7;
  b.data[1] = 42;
  return b.data[1];
}
```

```
55 8b ec                       prologue
c7 06 00 00 07 00              [_b + 0] = 7         ; b.tag (FIXUPP)
c7 06 04 00 2a 00              [_b + 4] = 42        ; b.data[1] (FIXUPP)
a1 04 00                       ax = [_b + 4]        ; same offset (FIXUPP)
eb 00 5d c3                    epilogue
```

Findings:
- `struct Box` layout: `tag` at offset 0, `data` at offset 2 (right
  after `tag`). `data[1]` is at offset 2 + 1×2 = 4 in the struct.
- `sizeof(struct Box) = 8` (2 for tag + 6 for data[3]). Embedded
  array adds `count × element-size` with NO padding.
- `b.data[1]` with const index folds to a single byte offset
  + FIXUPP at the struct's symbol. Same shape as nested struct
  fields (`2503`) and 2D-arrays (`2512`).
- The lvalue and rvalue uses of `b.data[1]` both emit `disp16=4`
  with FIXUPP `_b` — confirms BCC computes the offset once at
  parse time per access.


## Struct passed by pointer — `[si+disp8]` per field

Fixture `2556-struct-ptr-arg-obj`:

```c
struct Big { int x; int y; int z; int w; };
int sum(struct Big *p) {
  return p->x + p->y + p->z + p->w;
}
```

```
55 8b ec                       prologue
56                             push si
8b 76 04                       mov si, p             ; p in si
8b 04                          mov ax, [si]          ; p->x   offset 0
03 44 02                       add ax, [si+2]        ; p->y   offset 2
03 44 04                       add ax, [si+4]        ; p->z   offset 4
03 44 06                       add ax, [si+6]        ; p->w   offset 6
eb 00 5e 5d c3                 epilogue
```

Findings:
- The idiomatic "pass struct by pointer" pattern: load pointer to si,
  then access fields via `[si+disp8]` for each.
- Compare to **pass struct by value**: pushing 16 bytes per call vs
  pushing 2 bytes (the pointer). Always pointer is cheaper for
  structs > 2-4 bytes.
- `p->field` decodes to `[si + field-offset]` directly — no special
  `->` codegen, just pointer-deref + offset arithmetic folded.
- The AX-accumulator pattern flows through all 4 adds — first field
  loaded with `mov`, then each subsequent with `add ax, [...]` using
  direct memory source.
- Total body for 4 field sum = 14 bytes (load + 3 adds + epi).


## Union of int and char[2] — alias same offset, little-endian access

Fixture `2563-union-int-char-obj`:

```c
union U { int i; char c[2]; };
union U u;
int main(void) {
  u.i = 0x1234;
  return u.c[0] + u.c[1];
}
```

```
55 8b ec                       prologue
c7 06 00 00 34 12              [_u+0] = 0x1234      ; u.i write (FIXUPP)
a0 00 00                       mov al, [_u+0]       ; u.c[0]   (= 0x34, LSB)
98                             cbw
50                             push ax              ; spill
a0 01 00                       mov al, [_u+1]       ; u.c[1]   (= 0x12, MSB)
98                             cbw
8b d0                          mov dx, ax
58                             pop ax
03 c2                          add ax, dx
eb 00 5d c3                    epilogue
```

Findings:
- Union members alias the same storage: `u.i` and `u.c[0]` both at
  `_u+0`. Sizeof(union U) = 2 (max of member sizes).
- **Little-endian byte order confirmed**: writing `u.i = 0x1234`
  produces `34 12` in memory. So `u.c[0] = 0x34` (LSB),
  `u.c[1] = 0x12` (MSB). Same as long byte order.
- All access happens via direct moffs8/moffs16 (`a0`/`c7 06`) at the
  appropriate offset within `_u`. No alias-aware codegen; the parser
  type system tracks which member name maps to which offset.
- The sum `c[0] + c[1]` uses the **push/pop spill pattern** from
  `2558` — both bytes promoted via cbw, with stack-spill between
  them since both compete for AX.
- This is the canonical pattern for type punning via union in C.


## Array of struct with constant subscript — single byte offset

Fixture `2598-arr-of-struct-access-obj`:

```c
struct P { int x; int y; };
struct P arr[3];
int main(void) {
  arr[1].x = 7;
  arr[1].y = 9;
  return arr[1].x + arr[1].y;
}
```

```
55 8b ec                       prologue
c7 06 04 00 07 00              [_arr+4] = 7       ; arr[1].x
c7 06 06 00 09 00              [_arr+6] = 9       ; arr[1].y
a1 04 00                       ax = [_arr+4]      ; arr[1].x
03 06 06 00                    add ax, [_arr+6]   ; + arr[1].y
eb 00 5d c3                    epilogue
```

Findings:
- `arr[K].member` with **constant K** folds to a single byte offset:
  `K × sizeof(struct) + offset_of(member)`. Here:
  - `arr[1].x` = 1 × 4 + 0 = 4
  - `arr[1].y` = 1 × 4 + 2 = 6
- Both store and load use moffs16 form (`c7 06 disp16 imm16` for
  store, `a1 disp16` for load) with FIXUPP to `_arr`.
- `add ax, [moffs16]` for the second field uses ModR/M `06` (mod 00
  r/m 110 = `[disp16]`), 4 bytes total.
- `sizeof(struct P arr[3])` = 12 bytes in `_BSS`.
- Generalizes the offset-folding rule:

  | construct                    | offset       |
  |------------------------------|--------------|
  | `obj.f` (scalar struct)      | offset_of(f) |
  | `arr[K]` (array)             | K × elsize   |
  | `arr[K].f`                   | K × elsize + offset_of(f) |
  | `obj.sub.f`                  | offset_of(sub) + offset_of(f) |
  | `m[i][j]` (2D, const i,j)    | i × cols × elsize + j × elsize |

  All collapse to a single disp16+FIXUPP load/store.


## Local struct-to-struct copy `s2 = s1` — inlined load-pair / store-pair

Fixture `2611-struct-local-copy-obj`:

```c
struct P { int x; int y; };
int main(void) {
  struct P s1;
  struct P s2;
  s1.x = 7;
  s1.y = 9;
  s2 = s1;
  return s2.x + s2.y;
}
```

```
55 8b ec 83 ec 08              prologue + 8B (2 structs)
c7 46 fc 07 00                 s1.x = 7    ; [bp-4]
c7 46 fe 09 00                 s1.y = 9    ; [bp-2]
8b 46 fe                       mov ax, s1.y   ; load FIELD-2 first
8b 56 fc                       mov dx, s1.x   ; load FIELD-1 into DX
89 46 fa                       s2.y = ax      ; store FIELD-2 first
89 56 f8                       s2.x = dx      ; store FIELD-1
8b 46 f8                       mov ax, s2.x
03 46 fa                       add ax, s2.y
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Small (≤4B) struct-to-struct copy via direct name `s2 = s1` is
  **inlined**: NO `N_SCOPY@` call.
- Pattern: **load BOTH fields into registers BEFORE storing either**:
  - field2 → AX
  - field1 → DX
  - then field2 store, then field1 store
  This avoids load-store interleaving.
- **Field order is REVERSED in the load/store pair**: y first
  (into AX), x second (into DX). Same shape as the `*p1 = *p2`
  pointer-copy in `2495`.
- Source-name copy and pointer-deref copy emit identical bytes
  (modulo addressing modes for stack-local vs pointer-target).
- Locals layout (declaration order, highest address first):
  - s1@[bp-4..-1]: x@[bp-4], y@[bp-2]
  - s2@[bp-8..-5]: x@[bp-8], y@[bp-6]


## Caller receiving 4-byte struct return — DX:AX → store both fields

Fixture `2614-call-receive-struct-obj`:

```c
struct Pair { int a; int b; };
struct Pair make(void);
int main(void) {
  struct Pair p;
  p = make();
  return p.a + p.b;
}
```

```
55 8b ec 83 ec 04              prologue + 4B local p
e8 00 00                       call _make           ; (EXTDEF)
                               ; receive DX:AX:
89 56 fe                       p.b = dx    ; HIGH field at [bp-2]
89 46 fc                       p.a = ax    ; LOW field at [bp-4]
8b 46 fc                       mov ax, p.a
03 46 fe                       add ax, p.b
eb 00 8b e5 5d c3              epilogue
```

Findings:
- After calling a function returning a 4-byte struct, the **caller
  reads DX:AX as (HIGH = field2, LOW = field1)** and stores both
  to the destination struct's slots.
- Mirrors the producer-side findings from `2524`: callee puts
  `ax = field0, dx = field1`. Caller reverses: `[+0] = ax,
  [+2] = dx`.
- **NO N_SCOPY@ call at the call site** for 4-byte struct return —
  the register convention handles the value.
- The post-call writes-then-reads is unoptimized (writes ax then
  re-reads `[bp-4]` instead of using ax directly). Standard BCC
  "values flow through memory" pattern.
- No stack cleanup after the call — the helper consumes no args.


## `const struct P *p` + `p->x` — `mov ax, [si]` (offset 0 no disp)

Fixture `2624-const-struct-ptr-obj`:

```c
struct P { int x; int y; };
int get_x(const struct P *p) {
  return p->x;
}
```

```
55 8b ec 56                    prologue + push si
8b 76 04                       mov si, p
8b 04                          mov ax, [si]    ; p->x at offset 0
eb 00 5e 5d c3                 epilogue
```

Findings:
- `const struct P *` byte-identical to `struct P *`. The `const`
  qualifier emits zero bytes.
- **First field access (offset 0) uses no displacement byte**:
  `8b 04` is `mov ax, [si]` (ModR/M `04` = mod 00, r/m 100 = `[si]`).
  Total 2 bytes. Compare to non-zero-offset access (`8b 44 disp8`,
  3 bytes).
- This makes the FIRST field of a struct cheaper to access through
  a pointer than later fields.


## `make().a` — struct-return + field-zero access = ZERO extraction cost

Fixture `2629-struct-ret-field-obj`:

```c
struct Pair { int a; int b; };
struct Pair make(void);
int main(void) {
  return make().a;
}
```

```
55 8b ec                       prologue
e8 00 00                       call _make (FIXUPP)
                               ; DX:AX returned; AX = field0 = a
eb 00 5d c3                    epilogue
```

Findings:
- A struct-return function's `field0` is already in AX after the
  call (since DX:AX = HIGH:LOW maps field1:field0). Accessing
  `.a` (the first field) requires **zero additional bytes**.
- The `make().a` expression compiles to just the call — no field
  extraction. The call result IS the return value.
- For `make().b` (field 1), BCC would need `mov ax, dx` (2 bytes)
  to extract DX → AX. To probe.
- This is a **clean peephole**: struct-return functions are
  inherently efficient at returning their first int-field.


## `make().b` field-1 access — `call; mov ax, dx` (2 bytes extra)

Fixture `2634-struct-ret-field-b-obj`:

```c
struct Pair { int a; int b; };
struct Pair make(void);
int main(void) {
  return make().b;
}
```

```
55 8b ec                       prologue
e8 00 00                       call _make (FIXUPP)
                               ; DX = field1, AX = field0
8b c2                          mov ax, dx       ; field1 → AX (2B extra)
eb 00 5d c3                    epilogue
```

Findings:
- For struct-return functions, the DX:AX convention means:
  - **`fn().field0`** = 0 bytes extra (`2629`)
  - **`fn().field1`** = 2 bytes extra (`mov ax, dx`)
- This makes the FIRST field of a struct slightly cheaper to access
  immediately after a call than later fields. Source-side ordering
  can exploit this — putting the most-accessed field first.
- For struct > 4 bytes (uses `N_SCOPY@` + hidden return ptr), all
  field accesses would go through memory loads on the destination
  struct, regardless of field position.


## Union with mixed-size members — sizeof = max, all share offset 0

Fixture `2642-union-mixed-size-obj`:

```c
union U {
  char c;
  long l;
};
union U u;
int main(void) {
  u.l = 0x12345678L;
  return u.c;
}
```

`_BSS` for `_u`: 4 bytes (= sizeof(long), the largest member)

```
55 8b ec                       prologue
c7 06 02 00 34 12              [_u + 2] = 0x1234  ; long HIGH
c7 06 00 00 78 56              [_u + 0] = 0x5678  ; long LOW
a0 00 00                       mov al, [_u + 0]   ; u.c = LOW byte = 0x78
98                             cbw
eb 00 5d c3                    epilogue
```

Findings:
- `sizeof(union U) = max(sizeof(members))`. Here:
  - sizeof(char) = 1
  - sizeof(long) = 4
  - sizeof(union U) = 4
- All members start at **offset 0**. So `u.c` and `u.l` alias the
  same address. The char overlaps with the LOW byte of the long.
- Little-endian layout means `u.c` (after `u.l = 0x12345678`)
  reads the LSB = `0x78`.
- Members of different sizes coexisting: the smaller member just
  doesn't use the high bytes.


## Local array of struct on stack — contiguous layout, const-folded subscripts

Fixture `2645-local-struct-arr-obj`:

```c
struct P { int x; int y; };
int main(void) {
  struct P arr[2];
  arr[0].x = 1;
  arr[1].y = 4;
  return arr[0].x + arr[1].y;
}
```

```
55 8b ec 83 ec 08              prologue + 8B local (2 × 4 = 8)
c7 46 f8 01 00                 [bp-8] = 1    ; arr[0].x
c7 46 fe 04 00                 [bp-2] = 4    ; arr[1].y
8b 46 f8                       mov ax, arr[0].x
03 46 fe                       add ax, arr[1].y
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Local array of struct allocates `count × sizeof(struct)` bytes on
  the stack (here 2 × 4 = 8 bytes), placed at `[bp - 8]` (declaration
  order from highest address).
- Element-field offsets fold:
  - arr[0].x → [bp-8] (struct base + 0)
  - arr[0].y → [bp-6] (struct base + 2)
  - arr[1].x → [bp-4] (next struct + 0)
  - arr[1].y → [bp-2] (next struct + 2)
- All accesses use `[bp + disp8]` directly — no runtime index math
  for const subscripts.


## Struct-by-val param + multiple field accesses — independent `[bp+disp]` loads

Fixture `2648-struct-by-val-sideeff-obj`:

```c
struct P { int a; int b; };
int g;
int use(struct P p) {
  g = p.a;
  return p.b;
}
```

```
55 8b ec                       prologue
8b 46 04                       mov ax, p.a    ; [bp+4]
a3 00 00                       [_g] = ax (FIXUPP)
8b 46 06                       mov ax, p.b    ; [bp+6]
eb 00 5d c3                    epilogue
```

Findings:
- Struct-by-val params live at `[bp+4 ..]` (after the return
  address). Each field at its compile-time offset within the
  struct.
- Multiple field accesses are **independent loads** — no caching
  in a register across the intervening operation.
- `g = p.a` stores AX → global via `a3 disp16` (3 bytes) since
  the value is already in AX.
- The next access `p.b` re-loads from `[bp+6]` (overwriting the
  AX value used in the previous expression).


## `p->x = p->y + 1` — tight 6-instruction body

Fixture `2656-struct-arrow-self-obj`:

```c
struct P { int x; int y; };
void shift(struct P *p) {
  p->x = p->y + 1;
}
```

```
55 8b ec                       prologue
56                             push si
8b 76 04                       mov si, p
8b 44 02                       mov ax, [si+2]      ; p->y (offset 2)
40                             inc ax              ; +1 peephole
89 04                          [si] = ax           ; p->x at offset 0 (no disp)
5e 5d c3                       pop si; pop bp; ret  (void!)
```

Findings:
- Read `p->y` via `[si+2]` (offset 2 → disp8 form).
- Write `p->x` via `[si]` (offset 0 → no-disp form `89 04`, 2 bytes).
  ModR/M `04` = mod 00 r/m 100 = `[si]`.
- The `+ 1` triggers the **`inc ax`** peephole (1 byte) regardless
  of which side of the assignment.
- Void function → no `eb 00` before pop sequence.
- Body total = 12 bytes (push si + load p + load y + inc + store x +
  pop si + pop bp + ret). Very compact.


## 5-byte struct return — N_SCOPY@ + hidden ptr arg (confirms rule)

Fixture `2671-struct-5b-ret-obj`:

```c
struct Five { int a; int b; char c; };
struct Five make(void) { ... return f; }
```

```
55 8b ec 83 ec 06              prologue + 6B local (5B struct padded)
c7 46 fa 01 00                 f.a = 1     ; [bp-6]
c7 46 fc 02 00                 f.b = 2     ; [bp-4]
c6 46 fe 58                    f.c = 'X'   ; byte at [bp-2]
ff 76 06                       push word [bp+6]   ; caller dst SEG
ff 76 04                       push word [bp+4]   ; caller dst OFF
8d 46 fa                       lea ax, [bp-6]     ; src OFF
16                             push ss            ; src SEG
50                             push ax
b9 05 00                       mov cx, 5          ; count
e8 00 00                       call N_SCOPY@      ; (EXTDEF)
8b 46 04                       mov ax, [bp+4]     ; return dst OFF
eb 00 8b e5 5d c3              epilogue
```

Findings:
- 5-byte struct return uses **N_SCOPY@** + hidden ptr arg — same
  shape as `2526` (3-byte). Confirms the size-to-strategy map:

| sizeof | strategy |
|--------|----------|
| 1      | AL only |
| 2      | AX only |
| 3      | N_SCOPY@ + hidden ptr |
| 4      | DX:AX |
| ≥ 5    | N_SCOPY@ + hidden ptr |

  Only sizes matching integer types (1, 2, 4) get register returns.
- Local 5-byte struct rounds up to 6-byte stack reserve (even
  padding).


## `struct { long v; }` return — byte-identical to `long` return

Fixture `2674-fn-ret-struct-long-obj`:

```c
struct Pkg { long v; };
struct Pkg make(void);
int main(void) {
  struct Pkg p;
  p = make();
  return (int)p.v;
}
```

```
55 8b ec 83 ec 04              prologue + 4B local (= sizeof(struct Pkg))
e8 00 00                       call _make (FIXUPP, EXTDEF)
89 56 fe                       p.v.HIGH = dx     ; [bp-2]
89 46 fc                       p.v.LOW = ax      ; [bp-4]
8b 46 fc                       ax = p.v.LOW      ; (int) cast
eb 00 8b e5 5d c3              epilogue
```

Findings:
- `struct { long v; }` returned by value: 4 bytes, returned in
  **DX:AX** (same as raw `long` and `struct {int, int}` from
  `2532` and `2614`).
- Caller stores DX → [+2], AX → [+0]:
  - `p.v.LOW` at offset 0 of the long, same as offset 0 of struct
  - `p.v.HIGH` at offset 2
- Confirms the 4-byte register-return rule is **size-based, not
  field-structure-based**. Any 4-byte struct (long, two ints,
  4 chars, etc.) uses DX:AX.
- `(int)p.v` truncates by loading only `p.v.LOW` (the low word
  at offset 0).


## `p->data[K]` (struct ptr + array field + const subscript) — single offset fold

Fixture `2676-struct-ptr-arr-field-obj`:

```c
struct Bag { int n; int data[4]; };
int get(struct Bag *b) {
  return b->data[2];
}
```

```
55 8b ec 56                    prologue + push si
8b 76 04                       mov si, b
8b 44 06                       mov ax, [si + 6]    ; b->data[2]
eb 00 5e 5d c3                 epilogue
```

Findings:
- `b->data[2]` for struct Bag:
  - `b->data` starts at struct offset 2 (after `n` int = 2 bytes)
  - `[2]` adds `2 × sizeof(int) = 4 bytes`
  - Total offset from b's base: 2 + 4 = **6 bytes**
- Compiles to single `mov ax, [si + 6]` (disp8 form, 3 bytes).
- Confirms the offset-folding rule: any chain of struct-field +
  const-subscript folds to a single integer offset at parse time.

