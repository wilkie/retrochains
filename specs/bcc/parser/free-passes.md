# Free passes (batches that needed no codegen changes)

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## Free passes (no code changes needed)

Three more probes hit existing paths byte-exactly:

- `548` — int local compound mul `x *= 3;` — already routed
  through the imul-via-AX skeleton.
- `549` — `if (x == g)` (int local vs int global) — the generic
  `emit_compare` Ident-load + memory-source path handles the
  asymmetric operand types.
- `550` — global int initialized to a folded constant expression
  `int g = 2 + 3 * 4;` — `try_const_eval` already folds nested
  BinOps at parse time, so the slot emits `dw 14` directly.

## Free passes (no code changes needed)

Three more probes hit existing paths byte-exactly:

- `572` — `if (a || b)` between two int globals: the bare-ident
  short-circuit lowering already routed through `emit_cond_
  branch` and the established or-skeleton.
- `573` — `sizeof(int *)` returns 2: the parse-time
  `parse_type_name` already handles the `int *` declarator and
  `Type::Pointer(_).size_bytes()` is 2.
- `574` — `continue` inside a `while`: the planner's
  `continue_target_slot = check_slot` mapping for while was
  already correct (continue → top of test). Distinct from the
  for-loop case fixed in fixture 558.

## Free passes (batch 86)

Two more probes hit existing paths byte-exactly:

- `575` — `int g = 42; int x = g;` local init from a global: the
  initializer path already routes through `emit_assign` and the
  global-load codegen.
- `576` — `r = (a == b);` comparison-as-value: `emit_eq` already
  materializes the boolean into a register and the store path
  was unchanged.

## Free passes (batch 87)

Three more probes hit existing paths byte-exactly with no code
changes:

- `578` — `if (a <= b) return 1; return 0;` (int signed less-
  than-or-equal in if-cond): `emit_cond_branch` already lowers
  `<=` to `cmp; jg <else>` with the correct signed predicate.
- `579` — `return a >= b;` (int signed greater-than-or-equal as
  return value): `emit_ge` materializes the boolean into AX and
  the return path was unchanged.
- `580` — `int f(int a, int b) { return a + b; }` (two-int-arg
  callee + call from `main`): the cdecl call/return path
  already pushes args right-to-left and the two-arg parameter
  frame layout came out byte-exact (we'd previously tested 1-
  and 3-arg variants but not 2).

## Free passes (batch 88)

Three more probes hit existing paths byte-exactly with no code
changes:

- `581` — `if (a && b)` (bare-ident logical-and between two int
  globals): the and-skeleton (`emit_cond_branch` + cascading
  branch on zero) handles this just like the bare-ident or-form
  fixed in fixture 572.
- `582` — `g--;` (int global postdec used as statement): the
  postdec lowering already maps to `dec word ptr [_g]` for the
  statement context.
- `583` — `if (!(a < b))` (logical-not applied to a relational
  expression): `emit_cond_branch` already inverts the
  predicate, so `!(a < b)` lowers to `cmp; jl <then>` (the not-
  taken edge falls through to the else).

## Free passes (batch 89)

- `586` — `char a; char b; a=1; b=2; return a + b;` (char + char
  return): the char-add lowering through AL/AH widening already
  handled this; both chars promote to int per C90, the sum lands
  in AX, and `ret` returns it.

## Free passes (batch 90)

Two more probes hit existing paths byte-exactly:

- `587` — descending `for (i = 10; i > 0; i--)`: the for-loop
  planner already handles the postdec step and `i > 0` test
  shape.
- `588` — `int a; int b; ... return a > b ? a : b;` (ternary
  over int globals): `emit_ternary` materializes both branches'
  values into AX correctly.

## Free passes (batch 92)

- `594` — `int x; int y; x = 16; y = 2; return x >> y;`
  (signed `int >> int` with a non-constant shift count): the
  existing shift-by-CL lowering (`mov cl, byte ptr [y]; sar ax,
  cl`) already byte-matches BCC.

## Free passes (batch 93)

Three more probes hit existing paths byte-exactly with no code
changes:

- `596` — `int *p; p = &g; return p[0];` (int-pointer subscript
  read, K=0): the deref-through-register read path already
  emits `mov ax, word ptr [si]`, identical to `*p` since K=0.
- `597` — `int f(int *p) { return *p; } int main(void) { int x;
  x = 7; return f(&x); }` (passing `&local` as an int-pointer
  arg): `&x` forces `x` to a stack slot, `lea ax, word ptr [bp-
  N]` materializes the address, and the existing call path
  pushes it.
- `598` — `int main(void) { int x; x = 5; return x * x; }`
  (square of a local): the `imul <src>` path with a non-
  immediate source already handles this (both operands resolve
  to the same register-resident local — `mov ax, si; imul si`).

## Free passes (batch 94)

- `600` — `int a, b, c; a = 1; b = 2; c = 3; return a + b + c;`
  (multi-decl int locals on one line): the parser already
  handles comma-separated declarators in a single decl, and the
  locals planner allocates each in declaration order.

## Free passes (batch 95)

Three more probes hit existing paths byte-exactly with no code
changes:

- `602` — `return (a + b) * 2;` (parenthesized sum then `* 2`):
  the runtime sum lands in AX via `add ax, <src>` and the new
  `* 2` peephole from batch 91 turns the constant multiply
  into `shl ax, 1`.
- `603` — `int a; a = 5; ++a; ++a; return a;` (sequential
  preincs on the same local): each `++a;` lowers to a register
  `inc` independently.
- `604` — `char c; int n; c = 5; n = c; return n;` (int =
  char widening through assignment): `emit_assign_local`
  already loads the char with sign extension via `cbw` and
  stores the widened word.

## Free passes (batch 96)

Three more probes hit existing paths byte-exactly with no code
changes:

- `605` — `int x; int y; x = 12; y = 10; return x | y;` (int
  OR between two locals): the bitwise-op path already emits
  `mov ax, <left>; or ax, <right>` for int operands.
- `606` — `void f(void) { return; } int main(void) { f(); return
  0; }` (void function with bare return): the void-return path
  already drops the value-load and just emits the exit jump.
- `607` — `int f(char c) { return c + 1; } int main(void) {
  return f(5); }` (int return from `char + 1` arithmetic): the
  char-param load through DL/CBW widens to AX, then `inc ax`
  computes the int return value.

### Deferred from batch 96

Probed `char f(int x) { return x + 1; }` (a char-returning
function whose body computes `x + 1` from an int param). BCC
truncates the int param at the load — `mov al, byte ptr [bp+4];
inc al` — instead of `mov ax, [bp+4]; inc ax`. Both produce
the same low byte, but BCC's shape is 1 byte longer (`inc al`
is 2 bytes vs `inc ax`'s 1) and matches the function's char
return type. Implementing this would require routing char-
returning function bodies through AL where the source is a
narrow expression. Probe replaced with the `int f(char c)`
direction (mirror image) — that one works through existing
char-param widening.

## Free passes (batch 97)

- `608` — `for (i = 0; i <= 5; i++) sum = sum + i;` (`<=` in
  for-test): the for-loop check lowers `<=` to `cmp; jg
  <break>` correctly.
- `610` — `char c; char *p; p = &c; return *p;` (char pointer
  to a stack char-local): `&c` forces `c` to a stack slot,
  `lea ax, [bp-1]` materializes the address, and `mov al,
  byte ptr [si]` reads through the pointer.

## Free passes (batch 99)

- `614` — `return x / 7;` (int divide by const): the batch-98
  `Div` immediate path already covers this — `mov bx, K; cwd;
  idiv bx` with no `mov ax, dx` follow-up (quotient is already
  in AX).

## Free passes (batch 100)

- `617` — `int x; x = 0; if (!x) return 1; return 0;` (`!x`
  on an int local in if-cond): `emit_cond_branch` already
  inverts the test through the standard `or ax, ax; je
  <then>` shape.
- `618` — `int x; int r; x = 0; r = !x; return r;` (`!x` as
  a value): `emit_logical_not` materializes `1` or `0` into
  AX based on the operand's zero-test.

## Free passes (batch 101)

- `622` — `char c; c = 1; c |= 32; return c;` (char compound
  OR with constant): the existing char-register compound-
  bitwise path (`or <reg8>, K`) already handled this — sibling
  of fixture 556's `c &= 31`.

### Deferred from batch 101

Probed `int main(void) { int a[3] = {10, 20, 30}; return
a[1]; }` (int local array with initializer list). BCC stores
the initializer values in a `_DATA`-segment `d@w` block and
copies them into the stack frame at function entry via
`N_SCOPY@` (the same helper used for struct copies > 4 bytes).
Our codegen panics with "non-constant init for non-int-like
type Array { elem: Int, len: 3 }". Implementing this would
need the init-data emitter plus the prologue-time
`push ss; lea ax, [bp-N]; push ax; push ds; mov ax, offset
d@w; push ax; mov cx, <size>; call N_SCOPY@` shape — sizable.
Probe replaced with the char-compound-OR variant.

## Free passes (batch 102)

- `624` — `char c; c ^= 32;` (char compound XOR with const):
  the bitwise-compound path already emits `xor <reg8>, K`
  (sibling of fixture 556's `c &= 31` and 622's `c |= 32`).

## Free passes (batch 103)

- `626` — `return x << 4;` (int shift-left by 4): falls into
  the CL form (since K=4 ≥ 4 per the new unroll cutoff above)
  — `mov cl, 4; shl ax, cl`.

## Free passes (batch 104)

- `629` — `int x; x = 13; return x & 7;` (int AND with const
  small enough to fit imm16): the `AndAxImm16` form added in
  batch 97 already handles this (`25 07 00`).
- `631` — `int a; int b; ... return (a + b) / 2;` (sum then
  divide-by-const): the runtime add lands in AX; the const
  divide goes through the batch-98 `mov bx, 2; cwd; idiv bx`
  path. (Note: BCC does NOT use a `sar` peephole for divide
  by power of 2 here — same shape as `/ 7`.)

## Free passes (batch 105)

- `634` — `for (i = 1; i <= 10; i++) { if (i > 5) break; sum +=
  i; }` (for + nested-if + break + compound-add): the existing
  for-loop body emission already routes `break` from inside a
  nested if to the loop's break_target_slot, and the compound
  `+=` path emits `add <reg>, <op>` in place.

## Free passes (batch 106)

- `635` — `char c = -1; return c;` (char neg-literal init):
  the batch-105 char-init mask (`v & 0xFF`) handles the
  negative value cleanly — `mov byte ptr [bp-1], 255`.
- `637` — `int x; int y; x = 5; y = x * 3; return y;` (int
  mul-const stored to local): the batch-99 `mov dx, 3; imul
  dx` path routes through AX, then `mov word ptr [bp-N], ax`
  stores the result.

## Free passes (batch 107)

- `638` — `int x; x = 5; if (x != 0) return 1; return 0;`
  (int `!= 0` in if-cond): `emit_cond_branch` already emits
  `cmp ax, 0; jne ...` for the comparison-with-zero pattern.
- `639` — `int a; int b; ... if (a != b) return 1;` (int !=
  int): the standard cmp-as-branch path lowers `!=` to `cmp;
  jne` over the operand pair.

## Free passes (batch 108)

- `641` — `do { x++; } while (x != 5);` (do-while with `!=`
  test): the do-while lowering and `!=` branch combine cleanly.
- `642` — `char c; c = 16; c >>= 2;` (char compound right
  shift, K=2): the existing char compound shift path unrolls
  `sar al, 1` (signed) twice — sibling of fixture 535's
  `c <<= 2`.

## Free passes (batch 109)

- `644` — `int x; x = 5; x += x;` (self-compound-add): the
  compound-add path emits `add <reg>, <reg>` cleanly even
  when source and destination are the same.
- `646` — `if (x == 5 || x == 10)` (logical OR with two `==`
  cmps): the cmp-as-branch path lowers each `==` to `cmp; je`
  and the OR-skeleton wires them together.

## Free passes (batch 110)

- `647` — `return a * b + c;` (three-way arith, mul then add):
  combines the batch-99 `imul <src>` path with the batch-109
  RHS-clobbers-AX swap.

## Free passes (batch 111)

- `650` — `int x; int y; x = 5; y = -x; return y;` (neg of
  var stored to another local): `emit_unary_neg` materializes
  the negation in AX and the assign-local path stores it.
- `652` — `if (a + b > 10)` (if with arith compare): the
  comparison's left operand is a non-constant BinOp; the
  comparison path materializes both operands and emits the
  standard `cmp; jle <skip>` form.


## Free passes (batch 666)

- `2348` — `int fact(int n) { return n<=1 ? 1 : n*fact(n-1); }`
  (recursive factorial): re-confirms recursion = ordinary near
  `call` + `pop cx` cleanup; `imul si` form multiplies the return
  value by the enregistered `n`. (Originally covered by `2255`.)
- `2349` — `unsigned int x % 8` → `and ax, 7` (accumulator form
  `25 07 00`). Re-confirms the unsigned-mod-pow2 peephole.
  (Originally covered by `1935`.)
- `2350` — `signed int x % 8` does NOT use AND — emits
  `mov bx, 8 / cwd / idiv bx / mov ax, dx`. Confirms that the
  peephole gates on signedness (signed `%` of a negative value
  cannot be expressed as bitwise AND).

## Free passes (batch 667)

- `2351` — `do { sum += i; i++; } while (i < n);` (do-while with
  variable-RHS condition): `i` enregistered (SI), `n` on stack at
  `[bp-2]`. Loop tail uses `cmp si, [bp-2] / jl back-16` (reg-vs-mem
  cmp + short backward jl). No preamble jump, since do-while always
  runs the body first. Confirms tail-test loop template.
- `2352` — `struct Big { int a,b,c,d; }; struct Big make(void); x =
  make();` (function returning a large struct): re-confirms the
  hidden-dest-pointer ABI for return-by-value of structs > 4 bytes.
  EXTDEF table imports `N_SCOPY@`. Caller pushes the address of `x`
  as a hidden last arg (visible as `ff 76 06` — `push [bp+6]` — in
  the make body). Callee writes its local result through that
  pointer via N_SCOPY@.
- `2353` — `enum {N=10}; int arr[N]; while (i<N) { arr[i]=i+1;
  sum+=arr[i]; i++; }` (enum constant in array size + loop bound):
  the enum value folds to a literal `10` at parse time. The stack
  reserve is `83 ec 14` (= 20 bytes for 10 ints). The compare
  emits `cmp si, 10` (imm8-sext form `83 fe 0a`), not a symbol
  reference. Confirms enum constants are compile-time literals
  everywhere.
- `2354` — 4-level nested `if (a>0) { if (b>0) { if (c>0) { if (d>0)
  { return 100; } } } }` (deeply nested ifs with one common fail
  path): each `cmp [bp-N], 0 / jle target` carries its own
  forward-disp8 offset (`17, 11, 0b, 05`) and they all converge on
  the same `xor ax,ax / jmp epilogue` block — no label coalescing,
  pure structural lowering. Confirms nested-if lowering doesn't
  merge tails.
- `2355` — `~x` for int (`return ~x;`): single `f7 d0` (`not ax`).
  Confirms BCC's `~` for int = direct one-instruction encoding, no
  round-trip.
- `2356` — `int x = -42; return x >> 15;` (signed shift by 15 = bit
  width minus 1): emits `b1 0f / d3 f8` (`mov cl, 15 / sar ax, cl`)
  — the cl-form is used since N=15 is ≥ 4. Signed shift right uses
  `sar` (arithmetic), preserving sign. Confirms the
  N≥4-tips-into-cl-form threshold and signed-shift = sar selection.

## Free passes (batch 669)

- `2363` — `for (i = 0, j = 10; i < 5; i = i+1, j = j-1)` (comma
  operator in for init and update clauses): each comma-separated
  sub-expression is evaluated for side effect in source order. All
  three locals (i, j, sum) enregister into SI, DI, DX. Standard
  for-loop template (init, jmp test, body, update, test, jcc back).
- `2366` — `int arr[5] = {7, 8};` (partial array initializer at
  file scope): BCC emits the 2 explicit values plus 3 zero-fills
  via LIDATA records, matching the C standard. Symbol IS in PUBDEF
  (no `static`), consistent with [[static-fn-file-local]] /
  [[static-arr-file-scope]] which suppress it.
- `2367` — `r = a > b ? a++ : b++;` (ternary with postinc in each
  branch): the standard ternary materialization template runs each
  branch's `mov ax, regN / inc regN` (postfix = old value to AX,
  then bump the source). Result lands in AX then is stored to r.
  Re-confirms postinc + ternary mechanics.
- `2368` — `int a; int b; int c; a=1; b=2; c=3; return a+b+c;` all
  on one source line. Tokenization-only — identical OBJ to the
  newline-separated form (3-int stack frame `83 ec 06`, three
  `mov [bp-N], K` stores, then add chain). Confirms line breaks
  carry no semantic weight in BCC's lexer.

## Free passes (batch 670)

- `2369` — nested switches each with 2 cases (`switch (x) { case 1:
  switch (y) { case 1:... } } `): both switches under the
  4-case-contiguous dense threshold, so each uses the **linear
  cmp/je chain**. Each switch's `break` targets the END of THAT
  switch (correctly handles nested break propagation via separate
  exit labels).
- `2371` — `int sum(struct Pair *p) { return p->a + p->b; }`
  (passing struct via pointer instead of by value): callee
  enregisters `p` into SI; `p->a` = `mov ax, [si]`, `p->b` =
  `add ax, [si+2]`. Caller uses `lea ax, [bp-4] / push ax` to pass
  the address. (Avoiding by-value struct args since BCC hangs
  capture on `int sum(struct Pair p)` in our environment — passing
  small structs by value via DX:AX is documented elsewhere but the
  BCC -c capture path appears flaky there.)
- `2372` — `struct Buf { int len; char data[4]; }` (struct
  combining int and char-array members): layout is `len` at offset
  0 (2 bytes), `data[0..3]` at offsets 2..5. Total 6-byte struct
  fits in a 6-byte stack frame. Byte access uses
  `mov al, [bp+disp]` then `cbw` for int contexts; word access for
  `len` uses ordinary `mov ax, [bp+disp]`.

## Free passes (batch 671)

- `2376` — `int a[5]; p = &a[3];` (address-of array element with
  constant index): `lea ax, [bp-4]` — offset `bp - 4` is computed
  at parse time (bp-10 base + 3*2 stride = bp-4). No runtime
  stride mul. The pointer enregisters into SI. Re-confirms
  constant subscript = compile-time offset folding.
- `2378` — `struct Op { int (*fn)(int); }; op.fn = add5;
  op.fn(10);` (function pointer stored in a struct field, called
  through the field). Same `ff 56 disp` encoding as calling a
  stack-local function pointer — BCC treats the struct field as
  just another BP-relative memory operand. Re-confirms indirect
  call through any near memory operand.
- `2380` — `int * const p = &x;` (const POINTER, not pointer to
  const). The `const` qualifier on the pointer is parser-only;
  codegen is **byte-identical** to `int *p = &x;`. The qualifier
  enforces no-reassignment at parse time but generates no
  protection bytes. Re-confirms type qualifiers
  (`const`/`volatile`/`register`) carry no codegen weight beyond
  enregistration hints. Note: `*p = 42;` (writing through the
  pointer) is allowed since the const-ness is on the pointer
  itself, not the pointee.

## Free passes (batch 672)

- `2383` — `int (*pick(void))(int) { return doubled; } f = pick(); f(7);`
  (function returning a function pointer): the returned `doubled`
  address is `mov ax, offset doubled` (FIXUPP'd at link), then
  ordinary `89 46 fe` to save to `f`, then `ff 56 fe` indirect call
  via `f`. Re-confirms function-pointer return mechanics.
- `2385` — `r = (a++ > 0) ? b : c;` (postinc inside ternary
  condition): the postinc captures `a`'s old value into AX, then
  `inc si` bumps the enregistered `a`. The condition test
  (`or ax, ax / jle skip`) uses the captured old value. Standard
  ternary template otherwise.
- `2386` — `int r = (int)L;` (long-to-int narrowing cast): the
  cast is just a **low-half read**. With L's low half at `[bp-4]`
  and high at `[bp-2]` (little-endian halves), the cast emits
  `mov ax, [bp-4]` and the high half is discarded. Re-confirms
  long-to-int = drop the high word, no truncation work.

## Free passes (batch 673)

- `2388` — `int r = (a > b);` (bool-to-int materialization): the
  standard branching template lands a 0 or 1 in AX —
  `cmp/jle false; mov ax, 1; jmp end; xor ax, ax`. Re-confirms
  bool-to-int costs ~8 bytes and uses the false-branch zero via
  `xor ax, ax`.
- `2389` — `f(i--)` (postdec inside function-call arg): old value
  captured to AX before the decrement (`mov ax, si / dec si /
  push ax`), so the callee sees `i`'s old value while the caller's
  `i` reflects the decremented one after the call returns. Standard
  postdec-in-arg mechanics.
- `2390` — `struct Point pts[3];` accessed with constant indices
  (`pts[1].x`, `pts[2].y`): array-of-struct layout is flat
  consecutive — 3 × 4-byte structs = 12-byte stack reserve. Each
  `pts[K].field` folds at compile time to `[bp-12 + K*4 + offset]`,
  reachable as a single `mov ax, [bp+disp8]`. Re-confirms struct
  array layout has no inter-element padding.

## Free passes (batch 674)

- `2395` — `return (a = a + 1, b);` (comma operator in return
  expression): each comma-separated subexpression evaluated for
  side effect in order; only the last expression's value reaches
  AX. Standard comma semantics work in return position.
- `2396` — `add(dbl(3), dbl(5))` (nested function calls as
  arguments): R-to-L evaluation — `dbl(5)` runs first, its result
  is pushed on the stack as a save, then `dbl(3)` runs, then both
  results are pushed in argument order for `add()`. Re-confirms
  "chained calls bottom-up" arg-eval pattern.
- `2397` — `char *words[]; for (i=0;i<4;i++) sum += words[i][0];`
  (variable-indexed array of string pointers): the indexed pointer
  load uses `mov bx, [bx + offset_of_words]` (encoding
  `8b 9f disp16`) — a single combined instruction that adds the
  scaled index to the global array base and loads the pointer.
  Then `mov al, [bx]` derefs the loaded pointer for `[0]`. Confirms
  the `[bx+disp16]` ModR/M form is used when the global array base
  is FIXUPP-resolved.
- `2398` — `r1 = ++x; r2 = x++;` (pre/post-inc as RHS): pre-inc
  bumps then captures; post-inc captures then bumps. The ordering
  is visible as `inc si / mov r1, si / mov r2, si / inc si` —
  r1 gets the post-incremented value (since pre-inc happens
  first), r2 gets the pre-incremented value (since post-inc bumps
  after the store). Confirms pre/post-inc semantics for rvalue
  capture.

## Free passes (batch 675)

- `2400` — `char s[10] = "hi";` (char array initialized from a
  string shorter than the array): `_DATA` gets 'h', 'i', 0, then 7
  more zero bytes from a LIDATA fill. Accesses use the `a0 disp16`
  byte-load form. Re-confirms char-array-from-shorter-string =
  zero-pad rest.
- `2402` — `add(i++, j--)` (postinc and postdec in function-call
  args): R-to-L evaluation — `j` is captured + decremented first
  and pushed (= arg b), then `i` is captured + incremented and
  pushed (= arg a). Cleanup uses `59 59` (pop cx twice) for a
  4-byte cleanup, NOT `add sp, 4`. Confirms the cleanup-form
  threshold: 2-arg (4-byte) cleanup = pop cx × 2; ≥3-arg
  (≥6-byte) cleanup = add sp, imm8.
- `2403` — `int a[] = {7, 11, 13, 17, 19};` (array size inferred
  from initializer): `_DATA` holds 5 word values (10 bytes total).
  Symbol `_a` exported in PUBDEF. Accesses use the
  FIXUPP'd `[_a+disp]` forms (`a1 disp16` for `a[0]`, `add ax,
  [_a+8]` for `a[4]`). Re-confirms implicit-size + global array
  layout.

## Free passes (batch 676)

- `2405` — `int arr[] = {1+2, 3*4, 100-7};` (constant arithmetic
  in array initializer): each initializer expression folded at
  parse time to `3, 12, 93`. `_DATA` contains the raw word values;
  no runtime evaluation. Re-confirms constant folding extends
  through arithmetic operators in initializer contexts.
- `2410` — `typedef int A; typedef A B; typedef B C; C x;` (3-level
  typedef chain): resolves transitively to `int` at parse time.
  `C x;` has byte-identical OBJ to `int x;`. Confirms typedef is
  purely a name alias — chained or not, it carries no codegen
  weight.

## Free passes (batch 677)

- `2411` — `r = (a=1, b=2, c=3, a+b+c);` (4-element comma chain
  with side-effects + final value): each comma-separated expression
  evaluated for side effect L-to-R; only the last (`a+b+c`)
  produces the value. All three locals enregister (SI, DI, DX).
  Re-confirms comma operator left-to-right with last-value-wins.
- `2412` — `while (1) { if (i>5) break; sum+=i; i++; }`: `while
  (1)` emits no condition test at the top — body runs
  unconditionally with a backward `eb` at the tail. `break`
  forwards to the loop-end label via `jmp end`. Standard
  infinite-loop-with-break template (re-confirms earlier
  `while(1)` finding).
- `2413` — `x ? y ? 10 : 20 : z ? 30 : 40;` (triple-nested
  ternary): each ternary expands recursively to its own
  `cmp/jcc/mov/jmp/mov` skeleton — three independent expansions
  here. All result paths converge on a common end label. No CSE
  or tail-joining. Confirms ternary lowering is purely structural
  recursive.
- `2415` — `int i; char c; return i + c;` (mixed-width
  arithmetic): char is sign-extended via `cbw` then pushed; int is
  loaded; `pop / add ax, dx` produces the int result.
  Re-confirms the standard `cbw` widening for char-in-int-context.
- `2416` — `r = (a > 0) ? 100 : 200;` (ternary as initializer
  RHS): byte-identical to using the ternary as a statement RHS —
  the initializer context doesn't change the lowering. Standard
  ternary materialization template (cmp/jcc/mov/jmp/mov/store).

## Free passes (batch 678)

- `2417` — implicit function declaration: calling `trio(1,2,3)`
  before its definition in the same TU works under K&R C — `trio`
  is implicitly declared with return type `int`. Both `_main` and
  `_trio` appear in `PUBDEF`; the call uses the standard forward-
  resolved offset. Re-confirms BCC accepts implicit declarations.
- `2421` — `int arr[] = {10, 20, 30,};` (trailing comma in array
  initializer): the trailing comma is accepted; produces the same
  3-element array as without it. Re-confirms parser accepts C's
  optional trailing comma in brace-enclosed initializers.
- `2422` — `int **pp = &p; **pp = 99;` (double-pointer write):
  inner deref loads via `mov bx, [si]` (the address-of-p stored in
  pp), then outer write uses `mov [bx], 99`. Two levels of
  indirection = two memory accesses. Re-confirms pointer-to-pointer
  works through standard register-deref encoding.

## Free passes (batch 679)

- `2424` — `int m[2][3] = {{1,2,3},{4,5,6}};` (2D array with
  nested initializer): row-major flat layout in `_DATA` (6 words).
  Access `m[1][2]` folds to offset 10 (1*6+2*2). Nested
  brace-grouping `{{...},{...}}` is parsed correctly but produces
  the same flat data as `{1,2,3,4,5,6}`.
- `2425` — `sum7(int a..g)` (7-argument function): args land at
  `[bp+4]` through `[bp+10]` — all within disp8 range. Each `push
  ax` from caller is preceded by `mov ax, K`; cleanup uses
  `add sp, 14` (`83 c4 0e`) since 14 bytes > 4-byte pop-cx
  threshold. Re-confirms args at disp8 offsets and add-sp cleanup
  for many args.
- `2428` — `for (;;) { if (i > 3) break; i = i + 1; }` (empty for
  with break): standard infinite-loop template — no init/test/
  update preamble, just body + backward `eb` at tail. `break`
  becomes `jmp end`. Re-confirms [[for-loop-empty-cond]] from
  earlier work (fixture `720`).

## Free passes (batch 680)

- `2431` — `char strs[2][4] = {"AB", "CD"};` (2D char array init
  from string literals): laid out flat in `_DATA` as
  `41 42 00 00 43 44 00 00` (each string occupies its full 4-byte
  row, zero-padded). `strs[K][J]` accesses fold to offset
  `K*4 + J`. Re-confirms 2D char array layout = flat row-major with
  per-row zero padding.
- `2432` — `int sum_5(int a[5])` (sized array parameter): the size
  `[5]` is informational only — the param is treated as `int *`.
  Access `a[K]` for constant K folds to `[si + 2*K]`. Re-confirms
  array-param-decays-to-pointer.
- `2433` — `((((x + 1)))) * 2;` (deeply parenthesized expr): parens
  carry zero codegen weight. `(x + 1)` normalizes to `inc ax`; `* 2`
  normalizes to `shl ax, 1`. Total 5 bytes for the whole arithmetic.
  Re-confirms paren grouping is purely parser-level.
- `2434` — `if (p) ...` (pointer used directly as a boolean
  condition): compiles to `cmp word ptr [p], 0 / jz else` —
  standard null-pointer test. The pointer's full 16-bit value is
  compared to 0; NULL is `0x0000`.

## Free passes (batch 681)

- `2435` — iterating over an array of function pointers and
  calling each: `for (i=0; i<3; i++) sum += ops[i](10);` —
  variable-indexed fn-pointer call uses
  `shl bx,1 / mov bx, [bx + offset_ops] / push arg / ff 56 disp`
  pattern. Each iteration recomputes the indexed access. Re-confirms
  variable-indexed fn-pointer array invocation.
- `2436` — `struct Point pts[3] = {{1,2},{3,4},{5,6}};` (file-scope
  struct array with nested initializer): laid out flat in `_DATA`
  as 6 words. `pts[K].field` folds to `[pts + K*4 + offset]`. Symbol
  `_pts` in PUBDEF (no static). Re-confirms global struct array
  layout.
- `2437` — `do { if (i==3) break; i++; } while (1);` (do-while
  with `break` and constant-true condition): `while (1)` at the
  bottom emits NO test — just `eb f2` backward jmp. Same elision
  as while(1) at the top. `break` becomes `jmp end`.
- `2439` — `dbl(++i);` (pre-inc inside function argument): the
  pre-inc executes BEFORE the value is captured for the push.
  Order: `inc si / mov ax, si / push ax / call`. With i=5, the
  callee receives 6.
- `2440` — `single((a=5, a+2));` (comma operator in parenthesized
  arg position): the comma sequence is a SINGLE argument since the
  outer parens delimit a single expression. `a=5` runs for side
  effect, then `a+2` (= 7) is the value pushed. Also: `a+2`
  normalizes to `inc / inc` (2 bytes) since BCC prefers two `inc`
  over `add ax, 2`. Confirms the asymmetric small-add peephole
  (`+2 = inc inc` but `-2 = add ax, -2`, NOT `dec dec`).

## Free passes (batch 682)

- `2442` — `typedef int (*op_t)(int); op_t f = dbl;` (typedef for
  function-pointer type): byte-identical to declaring `int (*f)
  (int) = dbl;` directly. Typedef is parser-only — the alias
  resolves to the same fn-ptr type at codegen.
- `2443` — `x |= (1 << k);` with variable `k` (variable-shift bit
  set): emits `b8 01 00 / mov cl, k / d3 e0 / or si, ax` — the
  shift uses cl-form since `k` is runtime. Standard variable-shift
  bit-set idiom (compare with [[x-and-not-shift-K-fixture-2426]]
  for the constant-`k` fold).
- `2444` — `a = 30000; b = 5000; return a + b;` (runtime int
  overflow): values stored as 0x7530 and 0x1388; `add ax, [b]`
  produces 0x88B8 (= -30536 signed). No overflow check or warning
  — the hardware ADD wraps silently. Confirms [[int-overflow-wraps]]
  applies to runtime arithmetic as well as compile-time fold.
- `2445` — `int a, b, c = 7;` (multi-declaration where only the
  last has an initializer): a, b reserved without init; c
  initialized at decl. Stack layout assigns offsets in source
  order: `a→[bp-2], b→[bp-4], c→[bp-6]`. Only `c`'s `mov [bp-6],
  7` is emitted at decl-time; a and b get values later from the
  assignment statements. Confirms per-declarator initializer
  emission.

## Free passes (batch 683)

- `2447` — `r = (x = 5, x + 10);` (comma in initializer position):
  `x = 5` runs for side effect, `x + 10` (= 15) is the value
  stored to r. Standard comma operator. Re-confirms last-value-wins
  in initializer context.
- `2448` — `add((5), (7))` (redundant parens around call args):
  byte-identical to `add(5, 7)` — parens around individual args
  carry zero codegen weight. R-to-L push: `push 7 / push 5 / call /
  pop cx pop cx` (4-byte cleanup as `59 59`).
- `2451` — `char first(void) { return 'A'; }` returning char:
  `mov al, 0x41 / ret` — value lands in **AL** (low byte of AX),
  not full AX. Caller stores via `mov [bp-1], al` (byte store).
  Re-confirms char-return ABI: low byte in AL, high byte undefined.
- `2452` — `int a[3]; a[0] = x; a[1] = y; a[2] = x+y;` (local
  array filled via sequential assignment — not init list).
  Standard `mov [bp+disp], reg` for each store. No init-list
  short-circuit since assignments are statements, not initializers.

## Free passes (batch 684)

- `2453` — `int arr[5] = {100, 200};` (global array with explicit
  size > number of initializers): `_DATA` reserves 10 bytes; the 2
  explicit values plus 3 LIDATA zero-fills produce the standard
  partial-init layout. Same pattern as fixture `2366` but at file
  scope.
- `2454` — `int sum(int a[], int n)` (function with array param of
  unspecified size + count param): `int a[]` is byte-identical to
  `int *a` — the missing size is informational only. Standard loop
  with `a[i]` lowering to `[si + 2*i]` via shift+add.
- `2455` — `c = (char)i;` (int-to-char narrowing cast): emits
  `mov al, [bp-2]` (LOW byte of i) then `mov [c], al`. The cast
  discards the high byte by simply choosing the byte-width load
  instruction. No mask or truncation work needed.
- `2456` — non-void function without a `return` statement: AX
  contains whatever was last there (the local store via
  `mov [bp-2], 42` doesn't touch AX, so it returns whatever the
  caller left). Standard UB — re-confirms the documented "missing
  return = AX undefined" behavior.
- `2457` — `int x = ~0;` (bitwise NOT of zero): folds at parse to
  `0xFFFF` = -1. Emitted as `mov [bp-2], 0xFFFF`. Same constant-
  fold path as other `~K` cases.
- `2458` — `int x = compute();` (local initialized from a function
  call): standard `call / mov [bp-N], ax` pattern. The initializer
  context lowers identically to `int x; x = compute();`.

## Free passes (batch 685)

- `2459` — `volatile int v; a = v; b = v;` (two reads of a
  volatile local): each read emits `mov ax, [bp-2]`. Re-confirms
  volatile's main effect is **blocking enregistration**, not
  adding memory-barrier instructions. Since BCC doesn't CSE
  non-volatile reads either, the byte output here would be
  identical without `volatile` — but with volatile, the
  enregistration pool refuses the variable.
- `2460` — `(unsigned char)i` cast from int (i = 0x12FF, uc =
  0xFF): writes low byte via `mov al,[i] / mov [uc], al`, then
  reading back widens with `mov ah, 0` (zero-extend, NOT `cbw`
  sign-extend). Re-confirms uchar widening = `b4 00`, contrasting
  with signed char `98 (cbw)`.
- `2461` — `if (check(5)) ...` (function-call result as if
  condition): call result lands in AX, then `or ax, ax / jz else`
  pattern for the branch. Re-confirms call-as-cond mechanics.
- `2462` — `struct Big big = {10,20,30,40,50};` at file scope
  (5-int struct, 10 bytes in `_DATA`): packed sequential layout
  (offsets 0, 2, 4, 6, 8). LIDATA holds the 5 word values. Access
  via `[_big+offset]` with FIXUPP. Confirms 5-int struct = 10
  bytes (packed, no padding), same as smaller structs.
- `2463` — `my_strlen(char *s) { while (*s) { n++; s++; }
  return n; }` (manual strlen with deref-test + pointer-walk):
  standard pattern. `cmp byte ptr [si], 0 / jne loop / inc si /
  inc n`. Confirms manual strlen lowering.
- `2464` — `struct Vec { int arr[4]; }; v.arr[K] = N;` (array
  as struct member): `v` is an 8-byte stack slot;
  `v.arr[K]` for constant K folds to `[bp+disp]` with the
  combined struct+element offset. No special handling needed.

## Free passes (batch 686)

- `2465` — `int sum(int, int);` (forward prototype without parameter
  names): parser accepts; OBJ output byte-identical to a typed
  forward declaration with names. Parameter names are
  pure-syntactic in prototypes.
- `2466` — `int m[3][2] = {{1,2},{3},{5,6}};` (2D array with
  partial inner initializer): row 1 has only `{3}` provided; `m[1]
  [1]` zero-fills via LIDATA. Standard partial-init applies at the
  inner-level too.
- `2467` — nested-block variable shadowing: `int x; { int x; }`
  produces **separate stack slots** for outer and inner `x`. Stack
  layout: outer_x at `[bp-2]`, r at `[bp-4]`, inner_x at `[bp-6]`.
  Inner scope's `x` doesn't disturb the outer. Re-confirms
  [[nested-scope-shadowing]] finding.
- `2468` — `if (x > 30000)`: the constant 30000 exceeds disp8-sext
  range (max +127), so BCC emits the `81 /7 imm16` cmp form
  (`81 7e fe 30 75`) — 5 bytes vs the 4-byte `83 /7 imm8`. Confirms
  imm16 form selection for cmp with large constants.
- `2469` — `x * 100 + y * 1000` (two consecutive non-power-of-2
  multiplications): each uses `mov dx, K / imul dx`; intermediate
  result pushed on stack before the second mul; then `pop ax / add
  ax, dx` combines. Standard chained-mul pattern.
- `2470` — `int *arr[2]; arr[0] = &a; *arr[0] = 100;` (array of
  pointers, writing through deref of indexed element):
  `mov bx, [bp+arr0_offset] / mov [bx], 100` — load the pointer
  from the array slot, then deref-write. Standard ptr-array
  mechanics.

## Free passes (batch 687)

- `2471` — bitfield read/write: `struct { uint a:3; uint b:4; }`
  with `f.a = 5; f.b = 10;`. Standard bitfield packing applies
  (LSB-first within word). Re-confirms bitfield mechanics from
  earlier fixtures.
- `2472` — `int sum3(int *a)`-style array-as-pointer arg: caller
  passes `lea ax, [bp-N] / push ax`; callee accesses `a[K]` via
  `mov si, [bp+4] / mov ax, [si+2*K]`. Standard ptr-arg + indexed
  read.
- `2473` — `while (--i) body;` (pre-decrement as while
  condition): `eb fwd / body / dec si / jne back` — the jmp-to-
  test first; the dec sets ZF directly for the test. Identical
  template to the [[do-while-dec-only]] finding from fixture
  `2361`, modulo body placement.
- `2475` — `x = make();` (assigning from a function returning a
  small 4-byte struct): the struct fits in DX:AX per the ABI; the
  caller stores `[bp+a_off] = ax / [bp+b_off] = dx`.
- `2476` — `c ? (a=5, 10) : (b=7, 20);` (comma operator inside
  ternary branches): each branch evaluates its own comma sequence
  for side effect then provides the final value to AX. Standard
  ternary template + standard comma — composed cleanly.

## Free passes (batch 688)

- `2477` — `do { body; } while (0);` (do-while with constant-false
  condition): body runs exactly once; the test+back-jump are
  ENTIRELY ELIDED. Just body + epilogue. Re-confirms the
  while(0)-elision peephole.
- `2479` — `r = (a == b) == c;` (chained equality): first `a == b`
  produces bool 0/1 in AX, then compared against c via standard
  `cmp/jcc/mov 1/jmp/xor` template. No special chain folding.
- `2480` — mixed small + medium frame (106-byte total): all locals
  fit in disp8 range (max negative -106). Uses `83 ec 6a` for
  `sub sp, 106`; all accesses use disp8 ModR/M. Re-confirms the
  >127 threshold for tipping into disp16 [[large-frame-disp16]].
- `2481` — `x * 6` (non-power-of-2 mul): standard
  `mov ax, x / mov dx, 6 / imul dx` (4 + 3 + 2 = 9 bytes including
  the load). No LEA-based optimization since 8086 lacks the 32-bit
  scaled-addressing modes.
- `2482` — `struct Vec { int arr[3]; } v = {{10, 20, 30}};` (nested
  brace init for array inside struct): laid out flat in `_DATA` as
  3 word values. The inner braces are syntactic — same OBJ as
  `{10, 20, 30}` without the nested braces.

## Free passes (batch 689)

- `2483` — `char *empty = "";` (pointer initialized from empty
  string literal): `_DATA` stores 1 byte (NUL terminator); `empty`
  is a 2-byte pointer FIXUPP'd to the NUL's address. `empty[0]`
  reads the NUL = 0.
- `2485` — `x = (x | 0xFF00) & 0xFFFE; x = x ^ 0x000F;` (chained
  bitwise ops on a uint local): standard sequential codegen —
  `mov ax, x / or ax, 0xFF00 / and ax, 0xFFFE / mov x, ax / mov
  ax, x / xor ax, 0x000F / mov x, ax`. Each op produces an
  intermediate that's stored back, no CSE/fusion.
- `2486` — `sizeof("")` (sizeof empty string literal): returns 1
  (the NUL terminator). Compile-time fold. Re-confirms string
  literal includes its terminator in `sizeof`.
- `2487` — `int sum_grid(int g[2][3])` (2D array as function
  parameter): decays to `int (*g)[3]` (pointer to row of 3). Inside
  the callee, `g[0][0]` and `g[1][2]` fold to offsets `0` and `8`
  from the row-base pointer. Standard 2D-decay mechanics.
- `2488` — manual string-cmp loop with early-exit via incrementing
  `i` past the bound: stitches `for`-loop + `if`-inside-body + body
  manipulation of loop index. Standard.

## Free passes (batch 690)

- `2489` — `(x >> 4) & 0xF` (nibble extraction): unsigned shift
  via `mov cl, 4 / shr ax, cl` (cl-form since N ≥ 4), then `and ax,
  imm16` accumulator form (`25 0f 00`). Standard nibble-extract,
  no fusion possible on 8086.
- `2490` — calling `mystery(5)` before its definition (implicit
  declaration): same as fixture `2417` — BCC accepts; the function
  defined later in the file is reachable via the forward `e8`
  call. Both `_main` and `_mystery` in PUBDEF.
- `2491` — `(int)c` where c = 0xFF (signed char): sign-extends via
  `cbw` to `0xFFFF` = -1. Re-confirms char is signed by default;
  int-from-char widening goes through `cbw`.
- `2493` — `int sum_row(int (*row)[3])` (pointer to row of 3 ints):
  parameter is a 2-byte near pointer; `(*row)[K]` accesses use the
  pointer + constant offset. Caller passes `&m[1]` (= row 1
  address = `&m[0][0] + 6`).
- `2494` — `int *get_storage(void)` returning a pointer to a
  global: `mov ax, offset _storage` (FIXUPP'd) in the callee.
  Caller stores returned ptr to p, then writes through it.
  Standard fn-returning-ptr mechanics.

## Free pass: typedef of array == raw array in parameter slots

`typedef int Vec[3]; void f(Vec v)` and `void f(int v[3])` and
`void f(int *v)` all emit the same calling-convention bytes and
the same body for `v[i]`. We never need to track "is this a
typedef'd array" in the codegen-facing IR — by the time we reach
emit, the type has decayed to `int *` and the typedef name has been
resolved away. Fixture `2497-typedef-array-obj` confirms this for
sum-of-elements; the typedef adds zero bytes to the OBJ.

## Free pass: enum is just int

`enum E { A=1, B=2 };` followed by `enum E x; x = B;` is byte-identical
to `int x; x = 2;`. No metadata in the OBJ remembers the enum
identity; the only thing the parser uses enum for is binding the
identifier `B` to the integer 2 at lookup time. Fixture
`2496-enum-explicit-vals-obj` confirms with 0x14 (=20) appearing
inline as a 16-bit literal.

## Free pass: `sizeof(expr)` skips local reserve for unused vars

When the only mention of a local is inside `sizeof`, BCC's
liveness pass treats it as un-referenced and elides the stack
slot entirely (no `dec sp` / `sub sp`). The body becomes a single
`mov ax, K` from the folded sizeof. Fixture
`2498-sizeof-paren-expr-obj` is a one-instruction body proving
this. So sizeof is fully type-level, never producing any reference
that would mark a local as live.


## Free pass: `typedef struct {} T;` byte-identical to `struct T {};`

Fixture `2545-typedef-struct-obj`:

```c
typedef struct { int x; int y; } Point;
Point p;
int main(void) {
  p.x = 7;
  p.y = 9;
  return p.x + p.y;
}
```

emits the EXACT same bytes as:
```c
struct Point { int x; int y; };
struct Point p;
int main(void) {
  p.x = 7; p.y = 9;
  return p.x + p.y;
}
```

The typedef adds zero OBJ bytes; it's purely a name binding inside
the parser symbol table. By the time codegen sees `p.x`, it's
working with the offset 0 of a 4-byte struct — the `Point` name
has been resolved away.

This generalizes the earlier `Vec[3]` finding: typedef of array,
typedef of struct, typedef of primitives — all behave the same.
Codegen IR sees only the resolved type.

## Free pass: global init expression constant-folded

Fixture `2547-global-init-expr-obj`:

```c
int n = 2 + 3 * 4;     /* C90 requires constant init expression */
```

Lands in `_DATA` as the **single byte sequence `0e 00`** (=14, the
folded result). BCC evaluates `3 * 4 = 12` then `2 + 12 = 14` at
compile time and emits the literal. So the OBJ:

- Has `_n` in `_DATA` (not `_BSS`, since initialized)
- Carries 2 bytes of init data: `0e 00`
- Has NO init-time function or constructor

This means the parser's expression evaluator must fold constant-
expressions when computing initializers. Operator precedence
respected: `*` before `+`.


## Free pass: `si` and `di` are CALLEE-PRESERVED across calls

Fixture `2564-recursive-factorial-obj`:

```c
int fact(int n) {
  if (n <= 1) return 1;
  return n * fact(n - 1);
}
```

```
55 8b ec                       prologue
56                             push si             ; SAVE si (callee-preserved)
8b 76 04                       mov si, n           ; n in si
83 fe 01                       cmp si, 1
7f 05                          jg → ELSE
b8 01 00                       mov ax, 1           ; return 1
eb 0c                          jmp epi
                               ; ELSE:
8b c6                          mov ax, si
48                             dec ax              ; n - 1
50                             push ax
e8 e8 ff                       call fact (rel -24)  ; RECURSION
59                             pop cx
f7 ee                          imul si              ; ax × n (si still has n!)
eb 00 5e 5d c3                 epilogue (pop si included)
```

Findings:
- `si` is **callee-preserved**: pushed in prologue (`56`), popped in
  epilogue (`5e`). So a caller can keep a value in si across a
  function call WITHOUT manual spilling.
- Here `n` is loaded into si BEFORE the recursive call, and
  `imul si` AFTER the call works — si is still valid. The
  recursive `fact()` had its own `push si` in prologue to save
  whatever the caller's si was, then used si for its own n.
- Same applies to `di`. Both are part of the cdecl "non-volatile"
  set in BCC.
- `ax`, `cx`, `dx`, `bx` are caller-saved (volatile across calls).
- This **eliminates a class of spill/reload pairs** in our codegen:
  values placed in si/di before a call don't need to be saved by
  the caller.
- Recursive call uses `e8 rel16` with backward offset reaching the
  function's own entry point. The FIXUPP record handles symbolic
  resolution.


## Free pass: `register` keyword has no codegen effect

Fixture `2582-register-int-obj`:

```c
int main(void) {
  register int x;
  x = 7;
  x = x + 3;
  return x;
}
```

```
55 8b ec 56                    prologue + push si
be 07 00                       mov si, 7         ; x in si
8b c6 05 03 00 8b f0           x = x + 3 (AX-acc)
8b c6                          mov ax, x
eb 00 5e 5d c3                 epilogue
```

Findings:
- The `register` storage class is **purely a hint with no byte
  effect** in BCC. The same source without `register` produces the
  same OBJ when BCC chooses register-promotion (which it does for
  any hot local that has no address-of operator and a reasonable
  use count).
- So our parser can drop `register` after parsing — no need to
  carry it through to codegen IR.
- Tracks alongside `auto` (default) which also has no effect.


## Free pass: same-size signedness cast `(unsigned int)int` is a NO-OP

Fixture `2591-signed-to-unsigned-cast-obj`:

```c
unsigned int convert(int s) {
  return (unsigned int)s;
}
```

```
55 8b ec                       prologue
8b 46 04                       mov ax, s
eb 00 5d c3                    epilogue
```

The cast emits **zero bytes**. `(unsigned int)` and `(int)` differ
only in the type system; at the bit level a 16-bit signed and
unsigned value are identical. Same applies to `(unsigned char)char`
and `(unsigned long)long` for same-size conversions.

This generalizes: signedness in BCC is purely a parser-side concept
that influences operator selection (e.g. `jl` vs `jb` for compares,
`shr` vs `sar`, `mul` vs `imul`). The CAST itself never produces
bytes — it just changes how subsequent ops are emitted.


## Free pass: `const` qualifier has no codegen effect

Fixture `2618-const-char-ptr-obj`:

```c
int peek(const char *s) {
  return s[0];
}
```

```
55 8b ec 56                    prologue + push si
8b 76 04                       mov si, s
8a 04                          mov al, [si]
98                             cbw
eb 00 5e 5d c3                 epilogue
```

The body is **byte-identical** to the same function with `char *s`
(no `const`). The `const` qualifier emits zero OBJ bytes — it's
purely a type-system / read-only-enforcement concept that BCC
parses but doesn't propagate to codegen.

Joins the family of zero-byte qualifiers:
- `register` (`2582`)
- `const`
- `signed`/`unsigned` casts (same-size) (`2591`)
- `auto` (implicit default)
- `typedef` names (`2545`)


## `const int` parameter — byte-identical to plain `int`

Fixture `2757-const-int-param-obj`:

```c
int identity(const int x) {
  return x;
}
```

```
55 8b ec                       prologue
8b 46 04                       mov ax, x       ; just load and return
eb 00 5d c3                    epilogue
```

`const int x` byte-identical to `int x` — confirms `const`
qualifier is zero-byte for parameters too, joining the existing
free-pass family: `const char *` (`2618`), `const struct *` (`2624`),
`const int` global (`2662`).

