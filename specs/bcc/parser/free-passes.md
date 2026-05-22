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
