# Control flow (if/while/for/goto/return/&&/||/ternary)

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## `for(;;)` — empty condition

Fixture `507` (`int main(void) { int i; i = 0; for(;;) { if
(i > 5) break; i = i + 1; } return i; }`) — when the for's cond
is absent the trampoline `jmp short <check>` at loop entry is
elided. BCC layouts the body directly at the loop label and
falls through into the test/body without first jumping past the
nothing-to-check guard. `emit_for` now skips the trampoline
when `cond.is_none()`.

## Assignment expression in `if` condition

Fixture `513` (`if ((x = 5)) return x;`) — when the condition
is `AssignExpr`, BCC evaluates the assignment (storing the value
and leaving it in AX), then emits `or ax, ax` to set the flags
for the conditional branch. `emit_zero_test` now special-cases
`ExprKind::AssignExpr`: route through `emit_expr_to_ax` (the
chain-assignment path landed in batch 61) and append the `or
ax, ax` post-test.

## Empty statement

Fixture `522` (`for (i = 0; i < 100; i = i + 1) ;`) and `523`
(`while (g) ;`) — C90's null statement `;` was a parse error
because `parse_stmt` had no arm for bare semicolons. Added
`StmtKind::Empty` and an entry in `parse_stmt` that consumes the
single `;`. Codegen for `Empty` produces nothing (the loop's
back-edge / condition handling still runs because they're owned
by the surrounding `emit_for` / `emit_while`, not the body).
Adding the new variant required no-op arms in every match on
`StmtKind` (locals.rs use-counts, plan.rs label planner, emit_
s.rs call walker, codegen/mod.rs emit_stmt) — same pattern as
when `Goto`/`Label` were introduced.

## `return x++;`

Fixture `534` (`int x; x = 5; return x++;`) — worked on the
first try. The existing `emit_update_to_ax` already emits the
post-increment sequence `mov ax, <reg>; inc <reg>` and the
return path loads AX, which is exactly what BCC produces.

## `if (!x)` logical-not condition

Fixture `536` (`int g; if (!g) return 1;`) — `!x` in a
condition context lowers to the same flag-setting test as
`x`, but the conditional jump's polarity flips. `emit_cond_
test` now special-cases `Unary { op: Not, operand }` by
recursing on `operand` and swapping the returned `(true_mnem,
false_mnem)` tuple. Nested `!!x` collapses correctly through
the recursion. The actual asm output is exactly what the
unnegated test produces — only the JE/JNE pairing on the
caller side differs.

## `void` as a return type

Fixture `552` (`static void set(int *p) { *p = 99; }`) — parser
now accepts `void` as a return type. There's no dedicated
`Type::Void`; codegen treats functions with no `return <expr>`
statements identically regardless of declared return type, so
`Type::Int` serves as a placeholder. `parse_type` matches
`KwVoid` and the top-level type-probe in `parse_unit` includes
it.

While probing this, the publics-ordering rule revealed another
dimension: `void f(int *p)` + `int main(void)` (no statics)
emits `_main, _set` (forward), not the `_set, _main` reverse
that `int f(int *p)` would produce. Tested with many helper
names and the result depends on the helper's name in some hash-
bucket way we still can't characterize. Worked around by making
the helper `static` (which skips the PUBDEF emission entirely
and sidesteps the ordering question).

## `continue;` inside a for-loop — separate slot

Fixture `558` (`for (i = 0; i < 5; i = i + 1) { if (i == 2)
continue; s = s + i; }`) — the label planner reserved
`continue_target_slot` only when the body had *no* nested
labels (the "filler-slot" case for fixtures like 061). When the
body had nested labels (the `if` in 558 reserves two), the
planner re-used the next slot as both the continue-target *and*
the check-slot, so the emitter dropped two identical
`@1@N:` lines and `continue;` jumped to whichever the assembler
resolved first.

The fix: planner now runs a `body_has_continue` probe alongside
the body planning. When continue is present, it reserves a
distinct continue-target slot regardless of nesting. When
continue is absent and the body added no labels, it keeps the
historical filler reservation so the downstream label numbers
match the existing for-loop fixtures byte-for-byte. The
`body_has_continue` helper is duplicated into `plan.rs` (it
already lives in `codegen/mod.rs`); they walk the same Stmt
shape and need to agree.

## `if (f())` — call as boolean condition

Fixture `591` — `emit_zero_test` previously only handled `Ident`
and `AssignExpr`. Added a `Call` arm that lowers to `call near
ptr _f; or ax, ax`, matching BCC's pattern (the call leaves the
return value in AX and `or` sets ZF for the conditional branch).

## `while (1)` — frame-less infinite loop

Fixture `599` (`while (1) { if (g >= 3) break; g++; }`) — when
the while condition is a const non-zero, BCC elides both the
trampoline jump and the check label, leaving just `body_label /
body / jmp body_label`. Added a constant-cond branch at the
top of `emit_while`: when `try_const_eval(cond)` is `Some(v)`
with `v != 0`, emit the body with `continue_target = body_slot`
and a trailing `jmp body_slot`. The break-target label is still
gated on `body_has_break`.

## Chained `&&` / `||` — non-final operand short-circuit

Fixtures `620` (`if (a && b && c)`) and `621` (`if (a || b ||
c)`) — `emit_cond_branch` previously panicked with "nested
`&&`/`||` operators not yet supported". The recursive `&&`
case already inherited `(true_slot, false_slot)` for the right
operand, so chained `&&` was already correct once the assert
was lifted. The Or case was asymmetric: it passed `(None,
false_slot)` to the right, expecting the caller to emit the
true label immediately after — that's the "right falls through
on true" optimization for flat `a || b`. For chained
`(a || b) || c` (left-associative), the inner Or's right `b`
isn't the final operand: between b's evaluation and the true
label the outer Or emits `c`'s test, so b's "fall through on
true" lands in the middle of c's test. Fixed by distinguishing
final vs non-final Or via the outer `false_slot`: when
`false_slot.is_some()` we're at the top of an if-cond chain
(right can fall through); when `false_slot.is_none()` we're
inside another Or's LHS (right must jump on true to the
inherited `true_slot`).

## `char f(char c) { return c; }` — no widen on char return

Fixture `643` — char-returning function with a char-typed
return value. Our `emit_return_value_load` had a special
arm for unsigned char globals (no widen) but otherwise fell
through to `emit_expr_to_ax`, which widens char idents to AX
via `cbw`. BCC's ABI for char-returning functions is "AL
holds the value, AH is garbage" — the caller widens after
the call if it needs an int. Added a signed-char-return arm
that loads the char directly into AL (without cbw): `mov al,
byte ptr [bp+N]` for stack chars or `mov al, <reg8>` for
register chars. This addresses the deferred batch-96 item
about char-returning function bodies through AL.

## Recursion = regular call (no special handling); mutual recursion via fwd-decl; NO tail-call elimination

Fixtures `2255` (factorial), `2256` (mutual
recursion via fwd decl), `2257` (tail-call check
— BCC doesn't TCE) cover function-call recursion
patterns.

- `2255` (**recursive factorial**): just a normal
  `call near` to self. Each invocation gets a
  fresh BP frame via the standard prologue:
  ```
  ; In fact(int n):
  cmp si, 1                  ; n <= 1?
  jg L_recurse
  mov ax, 1                  ; base case
  jmp end
  L_recurse:
    mov ax, si
    dec ax                   ; n - 1
    push ax
    e8 [rel]                  ; call _fact (intra-TU)
    pop cx
    imul si                  ; ax *= n
  end:
  ```
  Recursion "just works" via the call/ret/BP
  discipline.
- `2256` (**mutual recursion via fwd decl**): the
  forward declaration `int is_odd(int n);` lets
  BCC's parser know is_odd exists when compiling
  is_even. Both fns end up in the same `_TEXT`
  segment with intra-TU `e8 [rel]` calls (filled
  in at compile-time once all symbols seen).
  No EXTDEF needed for forward intra-TU refs.
- `2257` (**no tail-call elimination**): `return
  helper(x)` lowers to full call + epilogue:
  ```
  ; In wrapper(int x): return helper(x);
  push word [bp+4]            ; arg
  e8 [rel]                    ; call _helper
  pop cx                       ; cleanup
  ; (no special handling — standard epilogue)
  mov sp, bp
  pop bp
  ret
  ```
  BCC does NOT collapse this into `jmp _helper`.
  Consistent with simple non-optimizing compiler.

**Recursion / call optimizations in BCC**:
| Optimization | BCC behavior |
|--------------|--------------|
| Tail-call elimination | Not performed |
| Tail-recursion → loop | Not performed |
| Inlining | Not performed |
| Common-subexpression elimination | Not performed |
| Dead-code elimination | Not performed |
| Constant propagation across blocks | Not performed |
| Loop unrolling | Not performed (except const-shift unroll for `<< 1` etc.) |

So calls have no special collapsing — every C
function call results in a real machine call,
prologue, epilogue, ret. Recursion goes through
the same call mechanism. Stack depth = recursion
depth × (BP saved + locals + ret addr).

For the Rust reimplementation:
- Recursive calls: emit standard call
  instruction; no special handling.
- Tail calls: do NOT collapse to jmp.
- Mutual recursion: track forward references in
  the symbol table; backpatch rel16 at EOF.

## `continue` in for = jmp to update; nested break = innermost end label only; `goto` = unconditional jmp

Fixtures `2228` (continue), `2229` (nested break),
`2230` (goto) cover the remaining control-flow
non-locals.

- `2228` (**`continue` in for-loop**): jumps to
  the **update** label (between body and test),
  NOT the test directly:
  ```
  ; for (i=0; i<10; i++) { if (i&1) continue; s+=i; }
  body:
    test ax, 1
    je not_odd
    jmp continue_lbl    ; <-- continue
  not_odd:
    add s, i
  continue_lbl:           ; update slot
    inc i
  test:
    cmp i, 10
    jl body
  ```
  So for is unique in having a separate
  continue-target. while/do-while continue jumps
  directly to the test.
- `2229` (**nested loop break, inner only**):
  each loop has its own end_of_loop label; break
  jumps to the **innermost** enclosing one:
  ```
  outer_body:
    inner_body:
      cmp j, 2
      jl skip
      jmp inner_end          ; break inner
    skip:
      ...
    inner_update / inner_test
    inner_end:
    outer_update / outer_test
  outer_end:
  ```
- `2230` (**`goto label`**): direct
  unconditional jmp to the label:
  ```
  ; goto done;  →  jmp done
  ; if (c) goto done;  →  cmp c / jcc-inverse skip / jmp done; skip:
  ```

**Control-flow non-locals summary**:
| Construct | Behavior |
|-----------|----------|
| `break` | `jmp innermost_loop_end` or `jmp switch_end` |
| `continue` (while/do) | `jmp test` |
| `continue` (for) | `jmp update` (separate label between body and test) |
| `goto label` | `jmp label` (direct unconditional) |
| `if (c) goto X` | `cmp c / jcc-inverse skip / jmp X / skip:` |
| `return` | `jmp fn_epilogue` (or fall through) |

**Why for needs a separate continue target**:
The for-loop's update step (`i++`) must run on
continue. In while/do, no update step exists, so
continue jumps directly to the test. The for-
specific label is the only loop-structural
difference between for and while.

For the Rust reimplementation:
- Maintain a stack of (loop_end, continue_target)
  labels for nested loops.
- break / continue emit `jmp` to the innermost
  matching label.
- Switch nests separately for break (switch_end
  label), but doesn't capture continue.
- goto: emit jmp directly to the label symbol.

## do-while = simplest loop form (no top jmp); for empty-init = jmp-test header; for empty-cond = unconditional jmp

Fixtures `2225` (do-while), `2226` (for empty
init), `2227` (for empty cond + break) cover the
remaining loop variants.

- `2225` (**do/while**): simplest loop layout —
  body first, then conditional jcc back:
  ```
  ; (init declarations done outside)
  body:
    body...
    cond_test
    jcc body
  ```
  No initial jump to test. Body runs at least
  once. For i=0..4 (5 iters): sum = 0+1+2+3+4=10.
- `2226` (**for with empty init**): standard for-
  loop layout — still has the `jmp test` at top:
  ```
  ; (init done before — empty here means no
  ;  additional init code in the loop)
  jmp test
  body:
    body
    update
  test:
    cond_test
    jcc body
  ```
  Empty init is a no-op slot, but the `jmp test`
  still emits to skip the body the first time.
- `2227` (**for with empty cond + break**): empty
  condition = always-true = unconditional `jmp
  body` at the bottom:
  ```
  body:
    body
    update
  jmp body         ; unconditional (empty cond = true)
  end_of_loop:
  ```
  Break translates to `jmp end_of_loop` — same in
  all loops.

**Loop layout summary** (final):
| Loop | Layout |
|------|--------|
| `while (c) b` | `jmp test; body: b; test: c → jcc body` |
| `do b while (c)` | `body: b; c → jcc body` (no top jmp) |
| `for (i; c; u) b` | `i; jmp test; body: b; u; test: c → jcc body` |
| `for (i; ; u) b` (empty cond) | `i; body: b; u; jmp body` (unconditional) |
| `for (; c; u) b` (empty init) | `jmp test; body: b; u; test: c → jcc body` |
| `for (;;) b` | `body: b; jmp body` |
| `break` | `jmp end_of_loop` |
| `continue` | `jmp test_or_update` |

So **while** and **for** share the same skeleton
(jmp-test-first) regardless of which clauses are
empty. The difference is just where init/update
go. **do/while** is unique in having no top jump.

For the Rust reimplementation:
- do/while: emit body THEN test.
- for/while: emit jmp-to-test at top.
- Empty cond: emit unconditional jmp instead of
  cond_test.
- break: jmp end_of_loop.

## Multi-arg printf R-to-L w/ natural sizes; `while(i--)` test-old/body-new; strcmp loop = nested byte cmps

Fixtures `2201` (printf mixed types), `2202`
(while postdec), `2203` (strcmp-like loop) cover
multi-arg push and loop idioms.

- `2201` (**printf with int, long, double mix**):
  args pushed R-to-L in natural sizes:
  ```
  ; Source: printf("%d %ld %f\n", i, l, d);
  
  ; Push d (rightmost):
  FLD m64 [d] / add sp, -8 / FSTP m64 [sp]    ; 8 bytes
  
  ; Push l (middle):
  push word [l.hi]                              ; HIGH first
  push word [l.lo]                              ; LOW second (lower stack addr)
  
  ; Push i:
  push word [i]                                  ; 2 bytes
  
  ; Push fmt addr:
  mov ax, 8 / push ax                            ; 2 bytes
  
  call _printf
  add sp, 0x10                                   ; cleanup 16 = 8+4+2+2
  ```
- `2202` (**`while (i--)` confirmed**): test uses
  OLD i; body uses NEW i (post-decrement):
  ```
  jmp test
  body:
    add di, si              ; sum += i (using NEW i)
  test:
    mov ax, si              ; capture OLD i
    dec si                  ; i--
    or ax, ax               ; test OLD
    jne body
  ```
  For i=10, body runs 10 times with NEW i = 9,8,...,0.
  Sum = 0+1+...+9 = 45.
- `2203` (**strcmp-like loop**): `while (*a &&
  *b && *a == *b) { a++; b++; }` lowers to a
  nested byte-test chain:
  ```
  loop:
    cmp byte [si], 0          ; test *a
    je L_exit
    cmp byte [di], 0          ; test *b
    je L_exit
    mov al, [si]              ; *a
    cmp al, [di]              ; *a == *b ?
    jne L_exit
    inc si / inc di           ; a++, b++ (post-cond)
  L_exit:
  ```

**Multi-arg push order — final**:
For `f(a, b, c)` cdecl with types (T1, T2, T3):
1. Push c (size of T3 first)
2. Push b (size of T2)
3. Push a (size of T1)
4. Call f
5. Cleanup `add sp, total_bytes` (or `pop cx × N` if ≤ 4 bytes)

Each long is pushed as `hi / lo` (so lo ends at lower offset).
Each double via `add sp,-8 / FSTP m64`.
Each int/ptr via single push word.

For the Rust reimplementation:
- Push args in source-right-to-source-left.
- For each arg, emit the per-type push sequence.
- Track total bytes for cleanup.

## Multi-fn PUBDEF = reverse-source, main last; implicit-int return ok; K&R fn syntax supported

Fixtures `2162` (3 fns + main PUBDEF order), `2163`
(implicit int return), `2164` (K&R-style decl)
cover three function-syntax behaviours.

- `2162` (**multi-fn PUBDEF order**): fns appear
  in PUBDEF as **reverse declaration order, with
  `main` last**:
  ```
  Source: a_fn, b_fn, c_fn, main
  PUBDEF: c_fn (offset 0x14), b_fn (0x0a),
          a_fn (0x00), main (0x1e)
  ```
  Helpers reversed; `main` placed at end. Each
  fn is its own PUBDEF record.
- `2163` (**implicit int return**): `helper(int x)
  { return x + 1; }` — no explicit return type
  defaults to `int` per K&R/C89. Same OBJ as
  `int helper(int x)`.
- `2164` (**K&R-style fn declaration**):
  ```c
  int add(a, b)
  int a, b;
  {
    return a + b;
  }
  ```
  Parameter declarations between the parameter
  list and body. Same OBJ as ANSI-style. BCC
  supports both styles.

**Function-syntax tolerance summary** (BCC accepts):
| Form | Status |
|------|--------|
| ANSI `int f(int a, int b)` | Standard |
| K&R `int f(a, b) int a, b;` | Supported (K&R-style) |
| Implicit-int `f(int x)` | Supported (K&R) |
| Implicit args `int f()` | Old-style, no arg checking |
| Function prototype `int f(int);` | Supported |
| Varargs `int f(int, ...)` | Supported |

**PUBDEF emission order** (combined):
| Symbol type | Order |
|-------------|-------|
| Variables (same segment) | Reverse declaration order |
| Functions (helpers) | Reverse declaration order |
| Function `main` | Always last in PUBDEF |
| Across segments | Within-segment order independent |

For the Rust reimplementation:
- Accept K&R fn syntax (or warn).
- Default return type = int when not specified.
- PUBDEF emission: reverse symbol list per
  segment, defer `main` to end.

## `extern fn` = EXTDEF + FIXUPP'd call; fwd-decl no impact; `static` fn = no PUBDEF, intra-seg call

Fixtures `2153` (extern fn), `2154` (fwd decl),
`2155` (static fn) cover function-symbol mechanics.

- `2153` (**`extern int printf(...)` call**): the
  external function generates an **EXTDEF** record
  in the OBJ symbol table. Call site uses `e8 00
  00` with FIXUPP to resolve at link time:
  ```
  ; symbol table:
  88 0a 00 07 _printf 00 71            ; EXTDEF
  
  ; main calls printf:
  mov ax, 0          ; b8 00 00 (FIXUPP for string)
  push ax
  call _printf       ; e8 00 00 (FIXUPP for fn addr)
  pop cx
  ```
  Varargs `(...)` in the prototype is just a
  type-check escape — doesn't affect call-site
  codegen for fixed-arity calls.
- `2154` (**forward decl**): `int helper(int x);`
  then later `int helper(int x) { ... }`. Forward
  decl is **purely type-system** — no codegen
  effect. Both `_helper` and `_main` are PUBDEF
  exports. Order in the OBJ matches source order.
- `2155` (**`static` fn**): file-local — **NO
  PUBDEF** for `_internal_helper`. Only `_main`
  is exported:
  ```
  ; PUBDEF section: only _main
  ; _internal_helper code is inline but invisible
  
  ; main calls helper via direct rel-near call:
  call _internal_helper       ; e8 e9 ff (intra-segment, NO FIXUPP)
  ```
  The static fn becomes invisible to other
  translation units. Call uses an internal
  relative offset — no link-time resolution
  needed.

**Function-symbol summary**:
| Modifier | OBJ effect | Call mechanism |
|----------|------------|-----------------|
| (default global) | PUBDEF (exported) | `e8 [rel]` intra-fn, or FIXUPP'd for ext |
| `static` | (not in PUBDEF) | `e8 [rel]` intra-segment |
| `extern` (no body) | EXTDEF (imported) | `e8 [rel]` with FIXUPP |
| Forward decl | (No symbol effect) | (Same as global) |

For the Rust reimplementation:
- `extern` w/ no body: emit EXTDEF; FIXUPP at
  call site.
- `static` fn: omit from PUBDEF; intra-segment
  call.
- Forward decl: type-system only.

## `goto label` = `jmp`; escape sequences decode at parse; octal literals (leading 0)

Fixtures `2108` (goto), `2109` (string escapes),
`2110` (octal/hex literals) cover three lexical
and control-flow patterns.

- `2108` (**`goto label`**): emits **unconditional
  `jmp`** to the label address. Same as a while-
  loop's back-edge:
  ```
  start:
    inc si              ; x++
    cmp si, 5
    jge skip
    jmp start           ; eb f8 (goto)
  skip:
  ```
  No structural difference between `goto` and an
  unconditional loop edge.
- `2109` (**string escape sequences**): `"\n\t\r
  \\\\"` decodes to `0a 09 0d 5c 5c 00` (with
  implicit null at end). All standard C escapes:
  | Escape | Value |
  |--------|-------|
  | `\n` | 0x0a |
  | `\t` | 0x09 |
  | `\r` | 0x0d |
  | `\b` | 0x08 |
  | `\f` | 0x0c |
  | `\v` | 0x0b |
  | `\a` | 0x07 |
  | `\0` | 0x00 |
  | `\\` | 0x5c |
  | `\"` | 0x22 |
  | `\'` | 0x27 |
  | `\?` | 0x3f |
  | `\xNN` | hex byte |
  | `\NNN` | octal byte |
- `2110` (**integer literals**): three syntaxes:
  - `0x...` (hex): `0xABCD = 0xABCD = 43981`
  - `0...` (leading 0 = **octal**): `0177 = 127`
  - `N...` (no leading 0 = decimal): `1000 = 1000`
  
  Each emits via `c7 46 disp imm16` for stack
  init.

For the Rust reimplementation:
- `goto`: emit `jmp` to the labelled address.
- Lex char escapes per the table above.
- Lex integer literals: detect `0x` prefix (hex),
  `0` prefix with no `x` (octal), otherwise
  decimal.

## `while(--x)` = `dec/jne` (no cmp); arr decays via `lea`; static int arr = `_DATA` init list

Fixtures `2045` (while predec), `2046` (arr decay
in fn call), `2047` (static int arr init) cover
three idioms.

- `2045` (**`while (--x)` = `dec / jne` only**):
  the dec instruction sets ZF; no separate cmp
  needed:
  ```
  jmp test
  body:
    inc si               ; count++
  test:
    dec di                ; --x (sets flags)
    jne body              ; loop while result != 0
  ```
  3 bytes for test+update (`4f / 75 fc`). For x
  = 5: 4 iterations (dec to 4,3,2,1 — all
  non-zero), exit on dec to 0.
- `2046` (**array decay = lea + push**):
  ```
  lea ax, [bp-6]           ; address of arr[0]
  push ax                   ; push the address
  ```
  Array name in expression context decays to
  pointer (= address of first element). `lea`
  computes the effective address; `push ax`
  pushes it.
- `2047` (**static int arr with init list**):
  values emitted in `_DATA` in order (`0a 00 14
  00 1e 00` = 10, 20, 30 little-endian).
  Access uses direct addressing:
  ```
  mov ax, [arr[0]]         ; a1 disp16 (FIXUPP)
  add ax, [arr[1]]         ; 03 06 disp16
  add ax, [arr[2]]         ; 03 06 disp16
  ```
  Static globals/locals live in `_DATA` and use
  the AX-form `a1`/`a3` for load/store (3 bytes)
  and the modrm-form `03 06 disp16` for add (4
  bytes).

For the Rust reimplementation:
- `while (--x)`: emit `dec / jne` directly (no
  preceding cmp).
- Array decay in call/expression: emit `lea` to
  compute address, then push.
- Static arr with init list: emit `_DATA` bytes
  in order; FIXUPP each access.

## Infinite loops `while(1)`/`for(;;)`/`do-while(1)` = body + jmp top; no test emitted

Fixtures `2039` (while(1)), `2040` (for(;;)),
`2041` (do-while(1)) confirm infinite-loop
codegen.

- `2039` (**`while (1) { body }`**): no test
  emitted at the top. Just body + unconditional
  `jmp` back:
  ```
  L_top:
    body
    jmp L_top                ; eb f6 (no test)
  L_break:
  ```
- `2040` (**`for (i=0;; i++) { body }`**): empty
  cond treated as always-true. Init runs first,
  then loop: body + update + jmp top.
- `2041` (**`do {body} while (1)`**): **byte-
  identical** to `while (1) {body}` — both
  compile to body + jmp top.

**Infinite-loop codegen summary**:
| Construct | Codegen |
|-----------|---------|
| `while (1) {body}` | `L_top:` body + `jmp L_top` |
| `do {body} while (1)` | (same — byte-identical) |
| `for (init;; update) {body}` | init + `L_top:` body + update + `jmp L_top` |
| `for (;;) {body}` | `L_top:` body + `jmp L_top` |
| `while (literal-non-zero N) {body}` | (probably same as while(1)) |

Always-true conditions elide the test entirely.
The cmp/jcc is eliminated at parse time.

For the Rust reimplementation:
- Detect literal-non-zero conditions in if/while/
  do/for; elide the test instruction.
- Always emit `jmp top` for the back-edge.

## String arg = FIXUPP offset push; sizeof types parse-time const; `return (a,b)` yields b

Fixtures `2027` (string arg), `2028` (sizeof
various types), `2029` (comma in return) cover
three remaining idioms.

- `2027` (**string arg to fn**): passing a string
  literal:
  ```
  mov ax, 0              ; b8 00 00 (with FIXUPP to string)
  push ax
  call _len_to_end
  pop
  ```
  String stored in `_DATA`; FIXUPP at the imm16
  resolves to the literal's offset at link time.
  
  Callee uses `while (*s++)` pattern: save s in
  bx, inc s, cmp byte [bx], 0, jne loop.
- `2028` (**sizeof types**): all values resolved
  at parse time:
  ```c
  sizeof(int) = 2, sizeof(long) = 4
  sizeof(char) = 1, sizeof(int *) = 2 (small model)
  ```
  Each emits `c7 46 disp imm16` storing the
  constant — no runtime sizeof computation.
- `2029` (**`return (a, b)` yields b**): comma
  in return evaluates both subexpressions in
  order, returns the value of the LAST:
  ```
  ; x = x + 1 (side effect)
  mov ax, si / inc ax / mov si, ax
  ; y = y * 2 (side effect + returned value)
  mov ax, di / shl ax, 1 / mov di, ax
  ; ax holds the last result (= new y)
  ret
  ```
  Standard comma operator semantics.

For the Rust reimplementation:
- String literal args: emit string in `_DATA`,
  push FIXUPP'd offset at call site.
- sizeof: resolve at parse time using the type
  table.
- Comma in return: emit all subexpressions; the
  last one's AX is the return value.

## `if(x)` ≡ `if(x!=0)` ident codegen; `while(x--)` captures-then-decs; arg eval R-to-L confirmed

Fixtures `2024` (if x vs if x!=0), `2025` (while
x--), `2026` (fn call side-effect args) confirm
three patterns.

- `2024` (**`if (x)` ≡ `if (x != 0)`**): both
  forms produce **IDENTICAL bytes** — `or si, si
  / je skip`. BCC recognises the explicit `!= 0`
  comparison as equivalent to truthiness.
  
  This means programmers can write either form
  with no codegen difference; BCC normalises
  both to the zero-test idiom.
- `2025` (**`while (x--)` captures OLD value**):
  ```
  body:
    inc si              ; count++
  test:
    mov ax, di          ; capture OLD x
    dec di              ; x-- (post-dec)
    or ax, ax           ; test OLD value
    jne body            ; loop while OLD != 0
  ```
  Critical: the test uses the **pre-decrement
  value**. For x=5, loop runs 5 iterations
  (testing 5,4,3,2,1); on x=0 the test sees 0
  and exits (x then becomes -1).
- `2026` (**arg eval right-to-left, confirmed**):
  `add(trace(1), trace(2))` evaluates:
  ```
  push 2 / call trace / pop      ; trace(2) first
  push ax                         ; save trace(2) result
  push 1 / call trace / pop      ; trace(1) second
  push ax                         ; save trace(1) result
  call add                        ; add(t1, t2)
  ```
  Right-to-left: trace(2) before trace(1).
  Matches the cdecl push order. Side-effects
  observable as right-to-left.

For the Rust reimplementation:
- Normalise `if (x)` and `if (x != 0)` to the
  same test (or reg, reg / jcc).
- `while (x--)`: emit capture-then-decrement
  before the test.
- Fn arg evaluation: emit subexpressions
  right-to-left, with each result pushed before
  the next is evaluated.

## `if (0)` skip via jmp; `if (1)` fall-through no test; `while (0)` jmp past body — bodies still emitted

Fixtures `2021` (if 0), `2022` (if 1), `2023`
(while 0) show **constant-condition control-flow
folding**.

- `2021` (**`if (0) ... else ...`**): emits
  unconditional **`jmp <else>`** at the top —
  no cmp/jcc test. Then-body is dead code in
  output:
  ```
  jmp L_else
  L_then: mov si, 99   ; dead
  jmp L_end
  L_else: mov si, 5     ; executed
  L_end:
  ```
- `2022` (**`if (1) ... else ...`**): NO cmp/jcc
  test, NO unconditional jmp. Just **fall-through
  to then**; jmp over else:
  ```
  mov si, 5             ; then-body, executed
  jmp L_end             ; skip else
  L_else: mov si, 99    ; dead
  L_end:
  ```
- `2023` (**`while (0) body`**): emits **`jmp
  past-body`** at the top — no init jmp / cmp /
  jcc structure. Body is dead:
  ```
  jmp L_end
  L_body: mov si, 99    ; dead
  L_end:
  ```

So constant conditions are **recognised at parse
time**, eliminating the cmp/jcc test, but **both
branches are still emitted** as dead code (no
DCE).

**Constant-condition control-flow summary**:
| Pattern | Codegen |
|---------|---------|
| `if (literal-0) T else E` | `jmp E` + then(dead) + jmp + else |
| `if (literal-1) T else E` | (no test) + then + jmp + else(dead) |
| `if (literal-0) T` (no else) | (skip via jmp) + then(dead) |
| `if (literal-1) T` (no else) | (no test) + then |
| `while (literal-0) body` | `jmp past-body` + body(dead) |
| `while (literal-1) body` | body + `jmp top` (infinite loop, no test) |
| `do {body} while (0)` | body + (no jcc, fall through to end) |
| `do {body} while (1)` | body + `jmp top` (infinite) |

For the Rust reimplementation:
- Constant-cond if/while: detect literal-0 / literal-1
  at parse time; emit direct jmp/fall-through.
- Don't perform DCE; emit dead bodies as-is.

## Const-expr fully folded; adjacent consts combined (`x+5+3`→`x+8`); `x && 0` → direct false-jmp

Fixtures `2018` (full const expr), `2019` (`x + 5
+ 3` const-combination), `2020` (`x && 0`) show
that BCC's parse-time folding is **more
sophisticated** than just literal identity.

- `2018` (**full const expression**): `(2*3) +
  (4*5) - 1` is fully computed at parse time:
  ```
  mov word [r], 0x19    ; r = 25 (direct constant store)
  ```
  All operators evaluated; only the final
  constant emitted.
- `2019` (**adjacent constants combined**): `x +
  5 + 3` parsed as `((x + 5) + 3)` but BCC
  **combines the constants** at parse time:
  ```
  mov ax, [x]
  add ax, 8             ; not 5+3 separately, but the sum 8
  ```
  So adjacent-constant folding works **across
  left-to-right associative expressions**, not
  just `K op K` cases.
- `2020` (**`x && 0` → direct false-jmp**):
  ```
  cmp [x], 0
  je  L_false           ; first operand's inverse-jcc
  jmp L_false           ; second operand is literal-0 → unconditional jmp to false
  L_true: mov ax, 1     ; dead code (unreachable)
  jmp end
  L_false: xor ax, ax
  end:
  ```
  The second operand being literal 0 produces an
  **unconditional `jmp L_false`** (since 0 is
  always false). The "true" branch becomes dead
  code in the output but is still emitted.
  
  So BCC recognises **literal boolean constants**
  in && and || and emits direct jumps, but
  doesn't eliminate the resulting dead code.

**Updated folding catalog**:
| Pattern | Effect |
|---------|--------|
| All-const expr | Computed at parse time |
| `x op K1 op K2` (associative) | Constants combined first |
| `x && literal-0` | Direct jmp to false branch (body dead) |
| `x || literal-1` | Direct jmp to true branch (else dead) |
| `0 + x`, `x + 0`, `x - 0` | → `x` (identity) |
| `0 * x`, `x * 0` | → 0 (zero-product) |
| `x ^ x`, `x - x` | NOT folded (no var-identity) |
| `x & -1` | NOT folded (only literal 0/1) |

For the Rust reimplementation:
- Full const expression folding via recursive
  evaluation.
- Combine adjacent constants in associative
  chains.
- Boolean literal in && / ||: emit direct jmp
  past the dead branch.
- Don't bother with DCE — keep emitting dead
  code as it appears in the source.

## uchar→int via `mov ah, 0`; bool arith materialises each cmp; empty-body while w/ side-effect-cond

Fixtures `1988` (unsigned char return), `1989`
(bool arith), `1990` (empty-body while) cover
three more idioms.

- `1988` (**unsigned char → int = `mov ah, 0`**):
  unsigned char return in AL only. Caller zero-
  extends with `mov ah, 0` (`b4 00`, 2 bytes):
  ```
  call _get_ub
  mov [c], al               ; byte store
  mov al, [c] / mov ah, 0   ; load + zero-extend
  ```
  Compare to signed char which uses `cbw` (1 byte
  sign-extend).
  
  **Char → int conversion summary**:
  | Source type | Extension | Bytes |
  |-------------|-----------|-------|
  | `char` (signed) | `cbw` | 1 |
  | `unsigned char` | `mov ah, 0` | 2 |
- `1989` (**bool arith — each cmp materialised**):
  `(a == b) + (a == c)` materialises **each
  comparison separately** via the full bool
  template, then sums:
  ```
  ; first cmp:
  cmp si, [b] / jne L_f1 / mov ax, 1 / jmp end1
  L_f1: xor ax, ax
  end1: push ax              ; save 1st bool
  ; second cmp:
  cmp si, [c] / jne L_f2 / mov ax, 1 / jmp end2
  L_f2: xor ax, ax
  end2: pop dx / add dx, ax  ; combine
  ```
  No fusion; each comparison generates a full
  template. Booleans treated as ints (0 or 1).
- `1990` (**empty-body while with side-effect**):
  `while (fn() < 5) ;` confirms the pattern:
  ```
  jmp test
  body:        ; empty
  test:
    call _inc_counter
    cmp ax, 5
    jl body
  ```
  No body instructions; just the test repeats
  until false. The fn call's side effect (++counter)
  is the only loop progress.

For the Rust reimplementation:
- Unsigned char → int: emit `mov ah, 0`; signed
  char → int: emit `cbw`.
- Bool arith in expressions: materialise each
  cmp via the value-context template.
- Empty-body loops: still emit `jmp test / body /
  test` skeleton.

## Unsigned `<=` uses `ja`; bounds check via short-circuit; `char` ret = AL only + cbw

Fixtures `1985` (unsigned `<=`), `1986` (bounds
check pattern), `1987` (char return) close out
common idioms.

- `1985` (**unsigned `a <= b`**): false-branch jcc
  is **`ja`** (`0x77`, unsigned above) — inverse
  of `<=`. Completes the unsigned-cmp jcc table.
- `1986` (**bounds check `i >= 0 && i < 5`**):
  short-circuit && with signed-cmp per operand:
  ```
  ; i >= 0 test:
  or si, si              ; cheap zero-test for i
  jl L_else              ; if i < 0, branch out
  ; i < 5 test:
  cmp si, 5
  jge L_else             ; if i >= 5, branch out
  ; ... bounds-check passed body
  ```
  Each operand's inverse-jcc goes to the same
  L_else target. `i >= 0` uses `or si, si` for
  the cheaper zero-test (since the constant is
  0).
- `1987` (**`char` return = AL only**): function
  returning `char` sets **only AL** (low byte of
  AX); AH is undefined. Caller:
  ```
  call _get_char
  mov [c], al              ; 88 46 ff — byte store
  mov al, [c]              ; 8a 46 ff — byte load
  cbw                      ; 98 — sign-extend to int (since char is signed)
  ```
  Char locals get **byte-sized stack slots** at
  odd offsets (e.g., `[bp-1]`).

**Unsigned-cmp jcc table** (complete, for false-
branch):
| Op | Unsigned false-jcc | Opcode |
|----|--------------------|--------|
| `<`  | `jae` (`jnc`)    | 73 |
| `<=` | `ja`             | 77 |
| `>`  | `jbe`            | 76 |
| `>=` | `jb` (`jc`)      | 72 |
| `==` | `jne`            | 75 |
| `!=` | `je`             | 74 |

For the Rust reimplementation:
- Unsigned-cmp jcc choice: use ja/jbe/jae/jb
  per operator (false-branch is inverse).
- char return: emit `mov al, val` only; caller
  treats AL as the byte result.

## Bool→int = full template; do-while-cmp loops back via fwd-jcc; string table = ptrs into `_DATA`

Fixtures `1919` (bool-to-int store), `1920`
(do-while with cmp), `1921` (string table) cover
three more idioms.

- `1919` (**bool→int store uses full template**):
  `int b = (x > 0);` (value context) emits the
  full materialization:
  ```
  or si, si              ; zero-test x
  jle L_false            ; inverse jcc (NOT >  is <=)
  mov ax, 1
  jmp end
  L_false: xor ax, ax
  end: mov [b], ax
  ```
  Same template for all comparison-to-int
  assignments. Contrast with **boolean context**
  (in `if`/`while`) which just emits cmp+jcc and
  doesn't materialize.
- `1920` (**do-while-cmp loops back via fwd-
  jcc**): `do { body; i++; } while (i < 10);`
  emits:
  ```
  body:
    add di, si      ; sum += i
    inc si          ; i++
    cmp si, 10
    jl body         ; loop while i < 10
  ```
  Note: **`jl` is the forward-sense jcc** here —
  jump-if-less = continue while i < 10. Do-while
  loops back when the condition is TRUE, so the
  jcc direction is **non-inverse** (matching the
  source-level comparator).
  
  Contrast with `if (i < 10) X;` where the jcc
  for the false-branch is `jge` (inverse).
- `1921` (**string table = ptrs into `_DATA`**):
  `char *table[3] = ...` assigns FIXUPP'd offsets
  to slots. The strings themselves are stored
  consecutively in `_DATA`:
  ```
  ; data:  AB\0CD\0EF\0  at offsets 0, 3, 6
  c7 46 fa 00 00         ; table[0] = "AB"@0
  c7 46 fc 03 00         ; table[1] = "CD"@3
  c7 46 fe 06 00         ; table[2] = "EF"@6
  ```
  Access `table[1][0]` is 2-step: load ptr from
  slot, then deref the ptr.

**Boolean vs value context revisited**:
| Context | Codegen |
|---------|---------|
| boolean (if/while/for cond) | cmp + jcc directly |
| value (assigned, returned) | cmp/jcc + mov ax,1/jmp/xor ax,ax template |

For the Rust reimplementation:
- Track expression context (boolean vs value); use
  different lowering for comparisons.
- do-while bottom-test uses fwd-sense jcc; while-
  top-test uses inverse-sense jcc.
- String table = array of offset-FIXUPPs into
  consecutive strings in `_DATA`.

## Empty fn keeps prologue/epilogue; `while(1)+break` no fusion; `continue` = goto update

Fixtures `1889` (empty function), `1890` (while(1)
with break), and `1891` (continue in for) cover
remaining control-flow shapes.

- `1889` (**empty function keeps prologue/
  epilogue**): `void noop(void) { }` still emits
  `push bp / mov bp, sp / pop bp / ret` (4 bytes
  total). No leaf-function optimization to skip
  BP setup.
- `1890` (**`while(1) + break` no fusion**):
  `while (1) { body; if (cond) break; }` emits
  ```
  top:
    body
    cmp [v], k
    jne L_skip          ; if NOT (cond), skip the break
    jmp L_break         ; the break
  L_skip:
    jmp top             ; loop back
  L_break:
    ; after loop
  ```
  Notable: BCC does **NOT fuse** `if (cond)
  break;` into a single `je L_break`. The `break`
  is compiled as a regular goto, with the if's
  own jcc chain preserved. This is wasteful — 7
  bytes where 5 would suffice. Consistent with
  BCC's "compile each statement independently"
  philosophy.
- `1891` (**`continue` = goto update**):
  `for (init; cond; update) { ...; continue; ...; }`
  with continue lowers to **jump to the update
  step**:
  ```
  body:
    ; ... if (i & 1) continue; ...
    test si, 1
    je L_not_odd
    jmp L_continue       ; continue → skip rest of body
  L_not_odd:
    ; rest of body
  L_continue:
    inc si               ; update
  test:
    cmp si, 10
    jl body
  ```
  So `continue` is **`goto <update-label>`** —
  not `goto <test-label>`. The for-loop's update
  always runs even on continue.

For the Rust reimplementation:
- Always emit prologue/epilogue, regardless of
  function body size or content.
- `while(1)` body + break: emit body, unconditional
  jmp back; `break` jumps to past-the-loop. Don't
  attempt to fuse `if-break` patterns.
- `continue` jumps to update-clause; `break` jumps
  past the loop entirely.

## `volatile` is no-op in BCC; do-while saves init jmp; forward decl resolves at parse

Fixtures `1886` (volatile int), `1887` (do-while
with zero test), and `1888` (forward fn decl)
cover three remaining type/control-flow shapes.

- `1886` (**`volatile` is effectively no-op**):
  `volatile int x = 0; x = 1; x = 2; return x;`
  emits **all three stores** plus the load.
  Notable: a non-volatile version would emit
  **identical code** because BCC **never performs
  dead-store elimination** (or any other
  optimization that would remove the qualifier's
  purpose).
  
  So in BCC 2.0, `volatile` is a **type-system
  marker** with **zero codegen effect** — the
  compiler already preserves all side-effects by
  default.
- `1887` (**do-while saves the init jmp**):
  ```
  ; no jmp to test at top
  body:
    add di, si      ; sum += i
    dec si          ; i--
    or si, si       ; test i (zero-test shortcut)
    jne body        ; loop while non-zero
  ```
  Saves 2 bytes vs while-bottom-test pattern (no
  `jmp test` at the top, since do-while
  guarantees ≥1 iteration). The test uses cheap
  `or si, si` since condition is just a variable.
- `1888` (**forward decl + later defn**):
  ```c
  int callee(int x);
  int main(void) { return callee(7); }
  int callee(int x) { ... }
  ```
  Compiles cleanly. The forward decl provides the
  prototype; main's `call _callee` uses a forward
  relative near-call (`e8 +disp`) since both
  functions are in the same segment. Symbol
  resolution happens at parse time.
  
  Note: function order in source = order in OBJ;
  main appears before _callee in symbol table.
  The forward call's displacement is resolved
  during codegen pass when the target function's
  position is known.

For the Rust reimplementation:
- `volatile` qualifier: trackable in type system,
  no codegen change (BCC never optimized stores
  anyway).
- do-while: emit body first, test at bottom,
  conditional jump back. No init jmp.
- Forward fn decls: resolve via two-pass parse
  (collect prototypes first) or back-patch
  during codegen.

## `(a && b) || c` precedence; `if (fn())` = call+or-ax; `!!x` folds in bool context

Fixtures `1862` (`a && b || c` precedence), `1863`
(`if (fn())`), and `1864` (`!!x`) cover the
remaining boolean idioms.

- `1862` (**mixed `&&`/`||` precedence**): parsed
  as `(a && b) || c` (`&&` binds tighter). The
  codegen converges short-circuit paths via
  forward jumps:
  ```
  cmp [a], 0
  je  L_test_c      ; a=0: && fails, try ||'s rhs
  cmp [b], 0
  jne L_true        ; (a && b) true: skip || rhs
  L_test_c:
  cmp [c], 0
  je  L_false
  L_true: ...
  ```
  Both `&&` operands' "false" paths land at the
  `||`'s rhs test; if the `&&` succeeds, jumps
  directly to true.
- `1863` (**`if (fn())`**): emits `call _yes / or
  ax, ax / je L_false`. The function's return
  value (in AX) is tested via the 2-byte zero-test
  `or ax, ax`. No special handling — same as `if
  (var)` when var is in AX.
- `1864` (**`!!x` folds in bool context**): `if
  (!!x)` lowers to **just** `cmp [x], 0 / je
  L_false / ...` — same as `if (x)`. BCC
  **recognizes the boolean-identity** at parse
  time, eliminating the double-negation sequence.
  
  Only when `!!x` is used **as a value** (e.g.,
  `int b = !!x;`) would the full `neg/sbb/inc /
  neg/sbb/inc` materialization sequence emit.

For the Rust reimplementation:
- Track the **context** (boolean vs value) when
  lowering `!`, `&&`, `||`. In boolean context,
  emit jcc-based short-circuit. In value context,
  materialize into AX via the bool-template
  sequence.
- `!!x` in boolean context: identity, just emit `x`'s
  test directly.

## 3-clause `&&` linearises; `!cmp` folds via inverted jcc; comma yields last operand

Fixtures `1859` (`a && b && c`), `1860` (`!(a <
b)`), and `1861` (`x = (n++, n++, n++)`) cover
remaining boolean/comma edge cases.

- `1859` (**3-clause `&&`**): emits **3 sequential
  cmp+je pairs** with progressively-shorter
  forward jumps to the same false-target (17, 11,
  5 bytes). Each subexpression tested in order;
  any zero skips to false. Standard `&&`
  linearisation extended to N clauses.
- `1860` (**`!(a < b)` folds via inverted jcc**):
  the `!` of a comparison is **simplified at parse
  time** — no boolean materialization needed.
  ```
  cmp ax, b
  jl L_false        ; flipped from jge — was "false-branch for <" = jge; with ! it inverts to jl
  ; true block
  ```
  Effectively: `!(a < b)` lowers to the same code
  as `if (a >= b)` (with the true/false branches
  arranged so the false-branch jcc is `jl`).
  
  General rule: **`!` on any comparison flips the
  false-branch jcc** at parse time, never
  computing the boolean value of the inner cmp.
- `1861` (**comma yields last operand**): `(n++,
  n++, n++)` discards intermediate values; only
  the **last operand's value** is yielded. With
  n=0 initial:
  ```
  xor si, si        ; n = 0
  inc si            ; 1st n++ (value discarded), n=1
  inc si            ; 2nd n++ (value discarded), n=2
  mov [x], si       ; x = 2 (pre-inc value of 3rd n++)
  inc si            ; 3rd n++'s post-inc, n=3
  ```
  Each subexpression emits its side-effect; the
  last one's value is captured into the
  assignment target (handled with the same pre-/
  post-inc capture-order rule).

For the Rust reimplementation:
- `&&` and `||` always emit short-circuit jcc
  chains, never materialise intermediate booleans.
- `!` on a comparison flips the jcc; never compute
  the boolean and negate.
- Comma operator emits each subexpr for side
  effects, captures only the last as value.

## Short-circuit `&&` chains `je/jne`; `||` jumps to true; each operand standalone tested

Fixtures `1856` (`a && b`), `1857` (`a || b`), and
`1858` (`x > 0 && x < 10`) characterise the short-
circuit boolean operators.

- `1856` (**`if (a && b)`**): lowers to
  **sequential cmp+je-on-false**:
  ```
  cmp [a], 0
  je L_false        ; short-circuit: if a == 0, skip
  cmp [b], 0
  je L_false        ; if b == 0, false
  ; fall through to true
  L_true: mov ax, 1 / jmp end
  L_false: xor ax, ax
  ```
  Both operands use the **same false-target**, so
  any zero in the chain skips. Classic short-
  circuit codegen.
- `1857` (**`if (a || b)`**): lowers to **jne-on-
  first then je-on-second**:
  ```
  cmp [a], 0
  jne L_true        ; short-circuit: if a != 0, skip to true
  cmp [b], 0
  je L_false        ; if b == 0, false
  L_true: mov ax, 1 / jmp end
  L_false: xor ax, ax
  ```
  The first operand jumps **forward to true** if
  non-zero; the second uses the standard false-
  branch. So `||` is encoded as "any non-zero
  wins early."
- `1858` (**`x > 0 && x < 10`**): combines `&&`
  with comparisons. Each comparison uses signed
  jcc (since x is signed):
  - `x > 0` → `or si, si / jle L_false` (uses
    zero-test shortcut for `>0`)
  - `x < 10` → `cmp si, 10 / jge L_false`
  Both branches go to a single false-target.

So the **logical operator codegen** is:
| Op | Pattern |
|----|---------|
| `&&` | both operands' inverse-jcc → same false-target |
| `||` | first operand's true-jcc → true-target; second operand's inverse-jcc → false-target |

This matches the inverse-condition pattern used
throughout BCC's branching.

For the Rust reimplementation:
- `&&` emits: lhs-cond / inv-jcc false / rhs-cond /
  inv-jcc false / true-block / jmp end / false-
  block.
- `||` emits: lhs-cond / true-jcc trueblk / rhs-
  cond / inv-jcc false / trueblk / falseblk.

## `while(--n)` = `dec/jne` (3B); `==`/`!=` inverse jcc; unsigned cmp uses `jae`/`jb`

Fixtures `1844` (`while (--n)`), `1845` (`==`/`!=`
materialization), and `1846` (unsigned `<`) confirm
several optimisation and signedness rules.

- `1844` (**`while (--n)` = dec + jne**): the loop
  test combines the decrement and zero-test into
  **`dec di / jne body`** (3 bytes total: `4f 75
  fc`). The `dec` instruction sets ZF based on
  the result, so the `jne` directly branches on
  it — no separate `cmp` needed. Beautifully
  compact loop test.
- `1845` (**`==` vs `!=` materialization**): both
  use the same boolean template (`cmp / jcc / mov
  ax, 1 / jmp / xor ax, ax`) but with **inverse
  jcc**:
  - `==` true → `jne` (75) for false branch
  - `!=` true → `je` (74) for false branch
  Consistent with the inverse-condition pattern
  applied throughout BCC's codegen.
- `1846` (**unsigned `<` uses `jae`**): for
  `unsigned a < unsigned b`, BCC emits **`jae`**
  (`0x73`, unsigned above-or-equal) for the false
  branch. Critical for correct unsigned semantics:
  `0x8000 < 0x0001` is FALSE unsigned (32768 > 1)
  but TRUE signed (-32768 < 1).

So **signedness drives jcc choice**:
| Op | Signed (jcc-false) | Unsigned (jcc-false) |
|----|--------------------|----------------------|
| `<`  | `jge` (7D) | `jae`/`jnc` (73) |
| `<=` | `jg`  (7F) | `ja`/`jnbe` (77) |
| `>`  | `jle` (7E) | `jbe`/`jna` (76) |
| `>=` | `jl`  (7C) | `jb`/`jc` (72) |
| `==` | `jne` (75) | (same) |
| `!=` | `je`  (74) | (same) |

So FP-cmp uses unsigned-flavour jcc too (per [[batch-
479-fp-cmp]]), matching the FPU's status-word
mapping via `sahf`.

For the Rust reimplementation:
- Combine dec+test loop conditions where the source
  is `while (--var)` or `do {...} while (--var)`.
- Choose jcc based on operand signedness (track
  signed vs unsigned types in the IR).

## Number bases parse-time uniform; for-update comma = 2 ops; char const = int

Fixtures `1826` (octal/hex/dec literals), `1827`
(comma in for-update), and `1828` (char escape
constants) cover three parser-level shapes.

- `1826` (**number bases**): `0x1F`, `077`, `42`
  all resolve to **same imm16 form** at parse
  time. Base prefix (`0x`, `0`, none) is
  consumed by the lexer; the OBJ stores binary
  values uniformly via `c7 46 disp imm16`. Source-
  level base is purely lexical convenience.
- `1827` (**comma in for-update**): `for (i=0,
  j=10; ...; i++, j--)` emits **both update
  statements sequentially** — `inc si / dec di`
  — at the loop's post-update point. No special
  comma handling; just statement sequencing.
  
  Also notable: with 3 multi-use locals (sum, i,
  j), all 3 enregister into DX, SI, DI. DX is
  used here for `sum` (1st declared with multiple
  reads) because the more common SI/DI got the
  loop induction variables.
- `1828` (**char constants**): `'X'`, `'\n'`,
  `'\t'`, `'\\'` are resolved at parse time to
  **int values** (10, 9, 92, 65 respectively).
  Stored via `mov word [m], imm16` since char
  literals have type `int` in C (per K&R / C89
  semantics). Escape sequences and printable chars
  follow the same parse-time resolution.

All three cases reinforce: **the C source-level
representation (base, escape, syntax form) is
purely lexical** — the OBJ contains only the
resolved binary values. BCC's parser does all
the resolution before codegen sees the values.

This matches a common rule across all the
constant-folding evidence: any expression composed
entirely of compile-time-knowable values is
reduced to a single binary constant before being
emitted. Source diversity → single binary form.

## Escapes parse-time resolved; nested ternary linear chain; args eval R-to-L

Fixtures `1823` (escape sequences in string),
`1824` (nested ternary), and `1825` (`sum3(i++,
i++, i++)`) close several remaining shapes.

- `1823` (**string escape sequences**): `"A\nB\t"`
  is resolved at parse time to bytes `41 0a 42 09
  00` in `_DATA`. `\n` → 0x0A, `\t` → 0x09. No
  codegen for escapes — purely a parser-level
  transformation.
- `1824` (**nested ternary**): `x<0 ? -1 : (x==0 ?
  0 : 1)` lowers to a **linear chain of cmp/jcc**
  with materialization into AX:
  ```
  or si, si
  jge L1            ; if NOT < 0
  mov ax, -1
  jmp store
  L1:
  or si, si
  jne L2            ; if NOT == 0
  xor ax, ax        ; 0
  jmp store
  L2:
  mov ax, 1
  store:
  mov [r], ax
  ```
  Nested ternaries don't get fused or specially
  optimized — just sequential evaluation.
- `1825` (**`sum3(i++, i++, i++)` arg order**):
  arguments are **evaluated right-to-left** and
  pushed in that order (matching cdecl R-to-L):
  - First evaluated/pushed: rightmost `i++` (i=5,
    push 5, inc to 6)
  - Second: middle `i++` (i=6, push 6, inc to 7)
  - Third: leftmost `i++` (i=7, push 7, inc to 8)
  
  In callee: `a` (leftmost) = 7 (last pushed),
  `b` = 6, `c` (rightmost) = 5. Sum = 18.
  
  Note: C's order-of-evaluation for fn arg
  expressions is **unspecified** in the spec — BCC
  chose right-to-left, matching the push order.
  Different compilers may differ.

For the Rust reimplementation:
- Resolve escapes at parse time, embed result
  bytes in `_DATA`.
- Lower nested ternary as a flat chain of
  cmp/jcc/jmp with single mov-target.
- Evaluate fn-call args **right-to-left** for
  side-effect-bearing expressions.

## `while(1)` ≡ `for(;;)`; nested loops separate; inner induction may win register

Fixtures `1802` (while(1) + break), `1803` (for(;;)
+ break), and `1804` (nested for loops) cover
remaining loop shapes.

- `1802` (**`while (1)`**) and `1803` (**`for (;;)`**)
  produce **byte-identical code shapes** — both
  emit the standard infinite loop:
  ```
  body:
  inc reg / ...      ; body
  cmp / jle continue
  jmp break_target
  continue:
  jmp body
  ```
  No conditional test before body; just unconditional
  jump back to body from continue point.
- `1804` (**nested loops**): standard structure
  with no fusion or special handling. Outer
  iteration `i` ends up on **stack** while inner
  iteration `j` got **DI** (register). With:
  - sum (1st declared): SI
  - i (2nd, outer-loop only): stack
  - j (3rd, inner-loop): DI
  
  Despite declaration order suggesting i should get
  the register slot, **the inner-loop induction
  variable won** — possibly due to loop-depth
  weighting in BCC's register allocator. Hot inner-
  loop variables get priority over outer overhead
  variables when register slots are limited.

This refines the register-allocation rule from
[[batch-482-register-allocation]]: among equally-
qualified candidates, **loop-depth weighting** can
override pure declaration order. Variables accessed
in deeper loops are weighted higher.

So the final register-selection priority:
1. `register` keyword (mandatory).
2. Highest read-count in expressions.
3. Loop-depth-weighted read count (inner loops
   count more).
4. Earliest declaration (tiebreak).

For the Rust reimplementation:
- Compute per-variable "weighted use count" =
  `sum over all reads of: 1 * (loop_depth + 1)`
  (or similar weighting).
- Select up to 3 highest-weighted into SI, DI,
  DX.

## `break` jumps to epilogue; `continue` jumps to post-update; `test` for bit check

Fixtures `1703` (do-while + break), `1704` (for +
continue), and `1705` (multi-decl init) cover three
control-flow shapes.

- `1703` (**`break` inside loop**): emits a
  **direct `jmp` to the loop epilogue** (or past
  the loop's test/end). Bypasses the loop
  condition entirely. The shape:
  ```
  ; loop body
  cmp di, 5         ; sum > 5?
  jle continue
  jmp break_target  ; -> after loop
  continue:
  cmp si, 10        ; loop test
  jl body
  break_target:
  ```
  So `break` is a one-byte `jmp short` for nearby
  loops, or `jmp near` (3 bytes) for distant ones.
- `1704` (**`continue`**): emits a **`jmp` to the
  loop's post-update / test step**, NOT to the
  loop body. So `continue` skips the rest of the
  body but still triggers `i++` (in a for loop)
  and re-tests the loop condition. The shape:
  ```
  body:
  test si, 1        ; check i & 1
  jz no_skip
  jmp continue_pt
  no_skip:
  add di, si        ; rest of body
  continue_pt:
  inc si            ; for's post-update
  test:
  cmp si, 10
  jl body
  ```
- `1704` also reveals **`test reg, imm`** for the
  bit check `if (i & 1)`. Opcode `f7 c6 01 00` =
  `test si, 1`. This sets ZF based on AND result
  *without* modifying SI — cheaper than `and si,
  1 / jz` because the destructive AND would require
  a temp. Then `jz` branches on the result. So
  bit-test patterns lower to:
  ```
  test reg, mask    ; f7 /0 + imm16
  jz / jnz target
  ```
- `1705` (**multi-decl init**): `int a = 1, b = 2,
  c = 3;` produces **byte-identical** code to three
  separate declarations. Each gets its own stack
  slot with its own `mov [m], imm` init. Multi-
  decl is a **parse-time syntactic shortcut** —
  fully expanded into separate declarations before
  codegen.
- `1705` also confirms: locals with only 2 uses
  (init + 1 read) **do NOT enregister**. The
  threshold for enregistration appears to require
  > 2 reads (or reads-across-statements), since
  these 2-use locals stay on stack.

Updated register-allocation rule:
- Enregister a local when **read-count ≥ 2** in
  expressions (NOT counting the init or single
  write). Initial declaration alone doesn't
  trigger enregistration even if it's followed by
  one read.

## `return a<b`; `(5,7)` drops LHS; `while(n)` uses `or si,si`

Fixtures `1661` (`return a < b;` direct return),
`1662` (`int x = (5, 7);` comma op in init), and
`1663` (`while (n)` truthiness) all pass on the
first capture.

- `1661`: a comparison result returned directly uses
  the **same boolean materialisation template** as
  if it were assigned to a variable. No "direct
  return" optimisation — `cmp / inv-jcc / mov ax,
  1 / jmp / xor ax,ax` always runs, and the result
  in AX is just used as the return register.
- `1662`: comma operator `(5, 7)` with constant LHS
  **drops the LHS entirely** at compile time. Only
  `mov [bp-2], 7` is emitted for `int x = (5, 7)`.
  Constant sub-expressions with no side effects in
  comma's left operand are discarded by the parser/
  AST. If the LHS had side effects (function call,
  assignment), it would have to be emitted —
  worth a future probe.
- `1663` (`while (n)`): standard bottom-test loop
  with **`or si, si / jne body`** as the truthiness
  test on register-resident `n`. Confirms the
  zero-test shortcut for enregistered locals from
  [[batch-414-cmp-zero-or-reg]] / fixture `1560`
  works in loop condition context too.

These three are all confirmations of previously-
identified patterns applied in slightly different
contexts. Useful for cross-checking but no new
findings.

## Indirect call via `ff /2`; `n--` returns old via `dec [mem]`

Fixtures `1658` (fn-ptr array indirect call), `1659`
(`while (decr())` fn-call cond), and `1660` (array
stores from binops) all pass on the first capture.

- `1658` (**indirect near call**): calling through
  a function pointer uses **`ff /2` (call near
  r/m16)** — specifically `ff 56 disp` for `call
  word [bp+disp]`. Same opcode family as data access
  (`ff` with /2 ModR/M selects "call indirect" vs
  /0 for inc, /1 dec, etc.). For an array of fn
  pointers, each call site emits `ff 56 disp` with
  the appropriate offset.
- `1659` (**`n--` global**): returning post-
  decrement of a global uses **`a1 [_n]`** (load
  AX from global) followed by **`ff 0e disp`** —
  `dec word [_n]` (opcode `ff /1` mod=00 rm=110
  with disp16 = direct memory). So `return n--`
  is a two-instruction post-decrement: load
  pre-value into return register, then `dec word
  [mem]` in place. No temp save needed since the
  return value was captured before the dec.
- `1660`: array stores from binops use the small-
  expression shortcuts in expression context:
  `a[2] = x - 1` lowers to `mov ax,si / dec ax /
  mov [bp-2], ax`. So `expr - 1` does use `dec
  ax` (1 byte) even in expression context — the
  longhand `i = i - 1` AX-roundtrip
  ([[batch-417-inc-dec-syntactic-split]]) was
  specific to the assignment IR shape.

The `ff /N` opcode family is now characterised:
| /N | Op             | Notes |
|----|----------------|-------|
| /0 | `inc r/m16`    | (used for memory inc) |
| /1 | `dec r/m16`    | (used for memory dec like `n--`) |
| /2 | `call near r/m16` | (indirect call) |
| /3 | `call far ptr16` | (far indirect) |
| /4 | `jmp near r/m16` | (computed jump — switch table) |
| /5 | `jmp far ptr16` | |
| /6 | `push r/m16`   | |

## do-while keeps body-first shape; side-effect in cond saves old value

Fixtures `1595` (`do { i++; } while (i < 3);`),
`1596` (`while (i < 3) { s += i; i++; }` multi-stmt
body), and `1597` (`while (i++ < 3);` side-effect in
cond) all pass on the first capture.

- `1595` (**finding**): `do { ... } while (cond)`
  is the **one loop form that keeps its own shape**.
  Lowering is `init / body / cmp / jcc back` —
  **no leading `jmp test`** like the
  while/for variants. The body runs once
  unconditionally, then the test follows. This
  matches the natural `do-while` semantics
  (post-test loop) and is distinct from the bottom-
  test pattern of the other forms.
- `1596`: multi-statement while body — standard
  bottom-test shape, both i and s enregister into
  SI and DI (both multi-use). Body is just two
  instructions (`add di, si / inc si`), then test.
- `1597` (**finding**): `while (i++ < 3)` with the
  side effect inside the condition lowers to:
  ```
  mov ax, si      ; save current i for compare
  inc si          ; i++ side effect
  cmp ax, 3       ; compare OLD i against 3
  jl back         ; loop if old i < 3
  ```
  The postfix-increment saves the pre-increment
  value into AX *before* applying the increment to
  SI, then compares the saved AX. This correctly
  implements the postfix `i++` semantics (uses old
  value, then increments). A *leading* `eb 00`
  (jmp to next instruction, 2 useless bytes) is
  emitted because the canonicalisation always
  inserts the "jmp test" at the top, even when the
  body and test are the same instructions — a
  systematic source of dead jumps.

Final loop-form lowering catalog (six base shapes):
| Form | Canonical lowering |
|------|--------------------|
| `if (cond) X else Y` | `cmp / inv-jcc L_else / X / jmp end / L_else: Y` |
| `while (cond) X`     | `jmp test / X: ... / test: cmp / jcc back` |
| `for (init; cond; incr) X` | `init / jmp test / X: incr / test: cmp / jcc back` |
| `do { X } while (cond)` | `X / cmp / jcc back` (no leading jmp!) |
| `while (1)` / `for (;;)` | `body / jmp back` (no test) |
| `do { X } while (0)` | `X` only (no overhead) |

## Bounded loops: `while`/`for` all canonicalise to bottom-test pattern

Fixtures `1592` (`while (i < 3) i++;`), `1593` (`for
(i = 0; i < 3; i++);` empty body), and `1594` (`while
(i < 3) { i++; }`) all emit **byte-identical code**
to each other. Combined with the existing for-loop
fixtures (`1205`, `1500`, etc.), the bottom-test
canonical pattern is now confirmed across all bounded
loop forms:

```
xor si, si      ; init
eb 01           ; jmp test
46              ; body: inc si  (or other body)
83 fe 03        ; test: cmp si, 3
7c fa           ; jl body  (back-edge with signed-less)
```

So BCC's loop normaliser unifies all of these into
the same shape:
| Source form | Internal IR |
|-------------|-------------|
| `while (cond) body` | `for ( ; cond ; ) body` |
| `for (init; cond; incr) body` | as-is |
| `while (cond) { body; incr; }` | same as for-loop |

The "incr" expression goes at body-tail (just before
test) regardless of whether it came from a for-incr
clause or was written explicitly at end of body.
The test goes at the bottom; entry is via `jmp test`
to skip the body before first iteration.

For the Rust reimplementation, this means the IR
must:
1. Rewrite `while (cond) body` as a for-loop with no
   init/incr but with same body.
2. Always emit bottom-test pattern with forward jmp
   on entry.

The earlier-batch finding that infinite-loop variants
([[batch-424-infinite-loops]]) all canonicalise to a
*top-test* pattern (since the cond is trivially true,
no test needs to be done; just jmp back from body
tail) is the degenerate case of this same
normalisation rule.

## Infinite-loop forms all canonicalise to identical bytes

Fixtures `1589` (`do { ... } while (1);`), `1590`
(`for (i=0; ; i++) { ... }`), and `1591`
(`for (;;) { ... }`) all pass on the first capture
and **emit byte-identical code** to each other and to
`1586` (`while (1) { ... }`). All four lower to:

```
prologue + push si
33 f6        xor si, si            ; i = 0
83 fe 03     cmp si, 3             ; loop_top:
75 02        jne body
eb 03        jmp loop_end          ; break
              body:
46           inc si                ; i++
eb f6        jmp loop_top
              loop_end:
8b c6        mov ax, si
eb 00        ret
```

So BCC's IR **canonicalises all "infinite loop" source
forms** (`while(1)`, `do...while(1)`, `for(;;)`,
`for(init; ; incr)`) into the same internal loop
shape: a test-position-at-top loop with the
`break`-cmp inside the body. Even the syntactic
difference between an explicit `for`-increment
clause and a body-tail post-increment collapses
into the same encoding.

This implies the IR has a **loop normaliser** that:
1. Recognises constant-true conditions and removes
   them.
2. Promotes the `for`-incr expression into the body
   tail (so the body becomes `body; incr;`).
3. Emits a single template: `init / test-loop_top:
   body / jmp loop_top / loop_end:`.

For the Rust reimplementation, the loop-IR layer
must perform this normalisation **before** codegen
to match BCC byte-exact for all infinite-loop
fixtures.

## const-cond loops: `while(1)` → `jmp` back; `while(0)` skips; `do…while(0)` no test

Fixtures `1586` (`while (1) { ... break; }`), `1587`
(`while (0) i++;`), and `1588` (`do i++; while (0);`)
all pass on the first capture, covering the
constant-condition loop forms.

- `1586`: `while (1)` lowers to an unconditional
  **`jmp` back to the loop top** — no cmp/jcc for
  the test. `break` is `jmp loop_end` jumping past
  the back-edge. Cleanest of the three patterns.
- `1587`: `while (0)` lowers to a forward **`jmp $+1`
  over the body** (dead code). The body `inc si`
  is still emitted but unreachable. Same shape as
  `if (0)` from `1585`.
- `1588`: **`do { ... } while (0)`** lowers to
  *just the body* — **no test or jump emitted**.
  This is the idiomatic "execute body exactly
  once" form used in macros, and BCC recognises it
  fully (test folded AND no back-edge generated).

So the constant-cond lowering table:
| Form | Lowering |
|------|----------|
| `if (1)` | true body, jmp over dead false body |
| `if (0)` | jmp over dead true body, false body |
| `while (1)` | body + jmp back (no test) |
| `while (0)` | jmp over dead body |
| `do…while (0)` | body only (zero overhead) |
| `do…while (1)` | (not yet probed; likely body + jmp back) |
| `for (;;)` | (not yet probed; likely body + jmp back) |

The do-while(0) case is the only one without dead
code emission — because there's no body to skip
(the body is what runs), and no back-edge to
generate (cond is false so no loop).

## const-arith folded; `if (1)`/`if (0)` test folded but dead code emitted

Fixtures `1583` (`int x = 100 - 7 * 3`), `1584` (`if
(1) return 5; return 10;`), and `1585` (`if (0)
return 5; return 10;`) all pass on the first capture
and characterise BCC's constant-folding scope.

- `1583`: full compile-time arithmetic folding —
  `100 - 7 * 3` reduces to **79 (0x4F)** stored
  directly into x's slot. The AST/parser layer
  evaluates constant expressions before reaching
  codegen.
- `1584`: `if (1)` lowers to **`mov ax, 5 / jmp $+5
  / mov ax, 10 / jmp epilogue`**. The test is
  folded away (no `cmp` / `jcc`), but the dead
  branch (`mov ax, 10`) is still emitted as
  unreachable code. The `jmp` skips 5 bytes — the
  exact length of the dead branch.
- `1585`: `if (0)` lowers to **`jmp $+5 / mov ax, 5
  / jmp epilogue / mov ax, 10`**. The test fold
  emits an unconditional `jmp` to skip the dead
  true branch, then falls through to the false
  branch.

So constant folding in BCC is **partial**: numeric
expressions are fully evaluated (as in `1583`); but
for `if (const)` the dead branch is still encoded as
unreachable code — only the *test* is skipped. The
encoder's IR doesn't have a "DCE after constant
fold" pass. The Rust reimplementation must match
this: emit both branches and connect them with the
appropriate `jmp` instead of cmp/jcc.

Combined with the [[batch-421-two-calltargets-strcond]]
finding that `if ("X")` is *not* folded at all
(emits the full template), the constant-folding
boundary is:
- Numeric/arithmetic operands: fully folded
- `if (numeric_const)`: test folded, dead branch
  kept
- `if (literal_string)`: not folded; full template

## 2 call-targets: decl order; call+binop chains; `if ("X")` not folded

Fixtures `1580` (2 multi-use locals both used as
call-targets), `1581` (`int x = seven() + 3` — init
from call-then-binop), and `1582` (`if ("X")` —
string-literal as if condition) all pass on the
first capture.

- `1580` (**resolves open question**): when 2 locals
  are both reassigned by call-returns (both
  "call-targets"), they get SI and DI in
  **declaration order** — `a` → SI, `b` → DI. The
  earlier hypothesis from [[batch-397-call-cross]]
  that "the call-target gets SI" only applied when
  *exactly one* of the multi-use locals is a
  call-target (in `1508`/`1510` only `c`/`d`
  respectively was a call-target; the non-call-
  target locals got DI). With multiple call-
  targets competing, plain declaration order wins.
- `1581`: a call result chains directly into a
  follow-on binop: `call _seven / add ax, 3 / mov
  [bp-2], ax`. No intermediate save — AX is the
  call's return register and stays live for the
  immediate `add`. So `f() + K` (or `f() op K` in
  general) lowers cleanly to call-then-op.
- `1582` (**missed optimisation**): `if ("X")` does
  **not** get folded to constant-true. BCC emits
  the full template: `mov ax, offset"X" / or ax,ax
  / je L_else / mov ax,1 / jmp / xor ax,ax`. The
  string-literal pointer is a known-non-null
  compile-time value (C guarantees it), but BCC
  doesn't recognise this in the IR — it emits the
  generic truthiness test. (Note: at runtime the
  test will succeed since linker resolves the
  pointer to a non-zero address, but the test is
  still wasted code.)

## char as arr idx, if-else with 3 locals enregistered, empty `void f()`

Fixtures `1493` (`int a[10]={0..9}; char c=3; return
a[c];` — signed char as int-array index), `1494`
(`int a=10, b=3; int x; if (a>b) x=a-b; else x=0;
return x;` — if-else with arith in both arms), and
`1495` (`void f(void){} int main(){f(); return 7;}` —
empty void function called from main) all pass on the
first capture. `1493` confirms signed-char-as-index
goes through `cbw`: `mov al,[bp-1] / cbw / shl ax,1
/ mov bx,ax / mov ax,[bx+_a]`. The char gets a 2-byte
stack slot (allocated by `dec sp / dec sp`) but only
the high byte `[bp-1]` holds the value — `[bp-2]` is
padding. BCC allocates a minimum 2-byte slot per
local even for a 1-byte type. `1494` shows BCC will
enregister *three* int locals when register pressure
allows: `a` → SI, `b` → DI, `x` → DX. DX is normally
a scratch register but BCC happily promotes a short-
lived local into it. The if-else lowers to `cmp si,
di / jle L_else / mov ax,si / sub ax,di / mov dx,ax
/ jmp / L_else: xor dx,dx / L_done: mov ax,dx`. The
`x = 0` arm becomes a one-cycle `xor dx,dx`. `1495`
confirms empty-body emission: `void f()` becomes
exactly 5 bytes — `55 8b ec 5d c3` (`push bp / mov
bp,sp / pop bp / ret`). The prologue is *not*
elided. `f` is still emitted as a PUBDEF. The call
site is `e8 disp16` with the standard near-relative
encoding/FIXUPP.

## `do { } while (0)`, `if ((a = b))`, chained 4-arm ternary

Fixtures `1433` (`int n=0; do { n++; } while (0);
return n;` — do-while with a constant-zero condition,
exercising the at-least-once semantic), `1434` (`int
a; int b=5; if ((a = b)) return a; return 0;` —
if-condition that contains an assignment, using the
assigned value as the truthy test), and `1435` (`return
a==0 ? 100 : a==1 ? 200 : a==2 ? 300 : 0;` — four-arm
chained ternary as the return value) all pass on the
first capture. `1433` confirms the do-while runs the
body once regardless of the test: n increments to 1,
then `cmp ...,0 / jne TOP` falls through. The
constant-folded `0` may or may not get short-circuited
to a hardcoded exit — the OBJ match shows BCC's
actual choice. `1434` confirms assign-in-if-cond: AX
gets the assigned value (5), `or ax,ax / je FALSE`.
`1435` confirms the right-associative ternary chain:
each `?:` is its own decision point, with the false
arm cascading to the next test. Result 300.

## `char *names[3]`, `(a==b) == (b<c)`, 4-way `if/else if/else`

Fixtures `1394` (`char *names[3] = {"hi", "ab", "x"};
return names[0][1];` — array of char-pointer init with
three string literals, then double-subscript), `1395`
(`if ((a == b) == (b < c)) return 1;` — equality
between two comparison results), and `1396` (`if (a==0)
return 0; else if (a==1) return 1; else if (a==2)
return 2; else return 3;` — four-way if-else-if chain)
all pass on the first capture. `1394` confirms global
array-of-pointers init: each pointer slot is initialized
with the address of its corresponding string literal,
laid out in the data segment. `names[0][1]` does two
deref-and-load: first `names[0]` = ptr to "hi",
second `[1]` = 'i' = 105. `1395` confirms compare-as-
int composed: each inner cmp materializes to 0 or 1
via sete-style boolean materialization, then the outer
`==` compares two int 0/1 values. Both inner are true
(1==1), so outer is true → return 1. `1396` extends
`1201`'s three-way pattern: each `else if` chains
through the same false-jump target, accumulating until
the final `else` catches the unmatched case. With a=2
the third arm fires.

## `a % 3`, `if (p != 0)`, char arr fill `'X'`

Fixtures `1364` (`int a=20; return a % 3;` — int mod by
non-pow2 const), `1365` (`int *p = &x; if (p != 0)
return *p;` — pointer-not-null check guarding a
dereference), and `1366` (`for (i=0;i<5;i++) buf[i] =
'X'; return buf[2];` — global char-array filled with
a constant via for-loop) all pass on the first
capture. `1364` is the mod counterpart to `1363`'s
divide-by-3: same `cwd / idiv` path, remainder in DX
moved into AX for return. 20 mod 3 = 2. `1365`
confirms `p != 0` lowers identically to plain integer
inequality: 16-bit cmp against zero, then `je FALSE
/ jmp TRUE` -- no special-cased "pointer" form. The
guarded `*p` then reads safely. `1366` confirms the
canonical buf-fill loop: index var `i` iterates,
`buf[i] = 'X'` stores `088h` byte through `mov
[bx+_buf],al` where `bx = i` (char-stride 1).

## `a && b || c`, tail-recursive `sumto`, `setBoth(&s,a,b)`

Fixtures `1358` (`if (a && b || c) return 1;` — mixed
short-circuit `&&` and `||` in one if-condition),
`1359` (`int sumto(int n, int acc) { if (n == 0)
return acc; return sumto(n - 1, acc + n); }` — tail-
recursive sum-of-1..n via accumulator), and `1360`
(`void setBoth(struct S *p, int a, int b) { p->x = a;
p->y = b; }` — function with struct-ptr arg writing
two fields) all pass on the first capture. `1358`
confirms `&&` binds tighter than `||` (standard C
precedence): the expression parses as `(a && b) ||
c`. With a=1, b=0, c=2: `(1 && 0) || 2` = `0 || 2` =
true, so return 1. The lowering uses standard short-
circuit jumps for each operator. `1359` confirms
tail-recursive call: the recursive call replaces the
return value, so each frame's epilogue immediately
unwinds back through the chain. Final answer
`sumto(5,0)` = 15. BCC does *not* tail-call-optimize
to a jmp; we see real call/ret pairs. `1360` confirms
3-arg fn with struct-ptr first and two ints: caller
pushes `b,a,&s` (cdecl reverse); callee does two
indirect stores through `[bp+p]`. Result 3+4 = 7.

## `if (a == -5)`, `unsigned char g = 200`, `buf[0]+buf[1]`

Fixtures `1340` (`int a=-5; if (a == -5) return 1;
return 0;` — int equality with a negative constant
in if-cond), `1341` (`unsigned char g = 200; return
g;` — global unsigned char initialized to a value
above 127), and `1342` (`char buf[3] = "ab"; return
buf[0] + buf[1];` — sum of two char-array elements
returned as int) all pass on the first capture.
`1340` confirms `cmp word ptr ...,-5` encodes the
negative as 0xFFFB sign-extended through the 16-bit
immediate. `1341` confirms unsigned-char init at 200
is just `db 0C8h` in the data segment -- no
sign-extension semantics for an unsigned type. On
return, AL=200, and `cbw` (signed-byte to int) would
turn it into -56 -- but BCC's char-as-int promotion
checks the type: for `unsigned char` we'd expect
`xor ah,ah` (zero-extend) instead. The match
indicates BCC's actual behavior here. `1342`
confirms char-array string init: `buf` gets `'a',
'b', '\0'`, and `buf[0]+buf[1]` promotes each to int
via `cbw`, sums to 97+98=195.

## Descending for-loop, `while (*++p)`, int from `-5` char

Fixtures `1310` (`for (i=5; i>0; i--) s += i; return s;`
— descending for-loop with post-decrement step), `1311`
(`p = "ab"; while (*++p) n++;` — while-loop walking
the string with prefix-increment-then-deref), and
`1312` (`char c = -5; int x; x = c; return x;` — int
local assigned from a negative-valued char, exercising
sign-extension) all pass on the first capture. `1310`
confirms the post-`--` step lowers to `dec word ptr
[bp-i]` and the test compares to 0 with `or ax,ax /
jng END` or the equivalent signed-comparison. Final
s = 5+4+3+2+1 = 15. `1311` confirms the prefix-inc-
deref idiom: each iteration `inc word ptr [bp-p]`
(char-stride 1) then loads byte via `[bx]` for the
test -- this is the C idiom for "skip the first char,
walk until null". `1312` confirms char-to-int assign
uses `cbw`: load `al` from the char slot (0xFB = -5
signed-byte), `cbw` extends to `0xFFFB` = -5 in AX,
then stored to the int slot. The return brings back
-5 which the harness encodes as exit_code 251 (=
256-5) for the shell.

## Fn `(int, char)`, for empty body, `while (i<j && i<3)`

Fixtures `1271` (`int f(int n, char c)` — function
with mixed-width parameters, called as `f(10, 5)`),
`1272` (`for (i=0; i<5; i++) ;` — for-loop whose body
is a single null statement), and `1273` (`while (i<j
&& i<3) i++;` — while loop whose condition is a
short-circuit `&&` of two compares) all pass on the
first capture. `1271` confirms the caller-side
char-arg promotion: BC++ 2.0 widens `5` to a 16-bit
push (cdecl assumes int-sized stack slots even for
char params), and the callee's `c` is read as a
word slot then `cbw`-promoted at use. So the
function-call ABI is "everything in stack as int-
sized words" regardless of declared param type --
matching K&R-era conventions. `1272` confirms a
null-statement loop body emits no body code: just
init, test/exit, step, and the back-edge jump --
the post-step rolls right into the test label. `1273`
confirms `&&` inside a while-condition short-circuits
the same as in an if: LHS comparison's false-jump
exits the loop directly, RHS test only happens when
LHS is true. No re-evaluation of LHS per iteration of
the body -- just the conditional cycle.

## Ternary as discarded side effect, `!!a`, int AND of two vars

Fixtures `1202` (`int a=3; a > 0 ? a++ : a--; return a;` —
the conditional is evaluated for its side effect with the
result discarded), `1203` (`int a=5; int b = !!a; return
b;` — double-negation as a 0-or-1 normalizer), and `1204`
(`int a=0xff; int b=0x0f; return a & b;` — basic `int`
AND between two locals) all pass on the first capture.
`1202` confirms that a ternary in statement position
lowers each arm into the same branch shape we use when
the result is stored, but the arm-result is then dropped:
no AX consolidation, just the side effect. `1203` shows
that `!!a` collapses to two `cmp/sete`-style boolean
materializations stacked back-to-back rather than being
short-circuited to a single normalizer — BCC takes the
expression as written. `1204` confirms our standard
binop-via-stack-spill path for `&` on two locals: LHS
into AX, push, RHS into AX, pop into DX, `and ax,dx`.

## Int preinc result used, char-to-int cast, three-way if/else

Fixtures `1199` (`int a=5; int b=++a; return b;` — int
prefix `++` used as RHS), `1200` (`char c=5; int x=(int)c;
return x;` — explicit char-to-int cast), and `1201`
(`if (a>0) return 1; else if (a<0) return -1; else
return 0;` — three-way if/else if/else chain) all pass on
the first capture. `1199` confirms that `int b = ++a;`
lowers the same as `++a; int b = a;` — pre-increment
writes the bumped value back to the slot and leaves it in
AX in time for the subsequent store. `1200` confirms that
explicit `(int)c` lowers identically to implicit
char-to-int promotion: a `cbw` on the byte loaded into
AL, no extra cast machinery. `1201` closes a coverage gap
for chained `if/else if/else`: each `else` branch flows
through the same return-epilogue join, with the BCC
tail-merge keeping a single `pop bp / ret` at the
function exit rather than per-arm epilogues.

## Global `++`/`--` in return and arithmetic

Fixtures `968` (`return g++;` — int global postinc in return),
`969` (`return ++g;` — int global preinc in return), `970`
(`return g++ + 1;` — int global postinc as an arithmetic
operand).

All three work end-to-end after batches 215/216. In return
position there's no follow-on `mov [bp-2], ax` store to
defer past, so the generic `emit_update_to_ax` shape (load
+ inc together for post; inc + load for pre) lands in AX
and the return path consumes it directly. No deferred-side-
effect peephole is needed because the function-exit jump
follows immediately.

For 970, BCC emits `mov ax, g; inc word ptr g; add ax, 1`
— the same load+inc pair from `emit_update_to_ax`, with
the binary `+ 1` becoming the standard `add ax, K` step.
The captured pre-update value flows into the arithmetic
unchanged. Byte-for-byte match.

Conclusion: the deferred-side-effect peephole from 963 /
966 is specific to the `<stack-local> = <global>++/--`
shape — when the use is a return, an arithmetic operand,
or a function call, the side effect naturally happens
before the value flows further, so the generic load+mutate
ordering matches BCC.

## Multi-var decl, `short`, `if` constant condition

Fixtures `929` (`int a, b; a = 3; b = 4; return a + b;` —
multi-variable declaration in a single statement), `930`
(`short s = 5;` — `short` keyword as a 16-bit int alias),
`931` (`if (1) { return 7; }` — literal-constant boolean
condition).

929 already works end-to-end — the parser's local-declaration
loop accepts a comma-separated declarator list and the locals
table allocates two distinct slots, both initialized by the
subsequent assigns.

930 needed one lexer change: the BC2.0 dialect accepts `short`
everywhere `int` does and produces the same 16-bit storage.
Rather than adding a separate `KwShort` token and threading it
through every type-parsing site (declarations, casts, sizeof,
function returns, struct fields, …), we map `short` directly
to `TokenKind::KwInt` in `lex_ident`. This collapses `short` /
`short int` / `unsigned short` into the existing `int` paths.
The downside is `short int s;` would lex as `int int s;` and
hit the dispatcher's "type at top level" failure — but no
current fixture pairs the two keywords. When one shows up, we
either add a dedicated `KwShort` or special-case the lexer's
buffer to skip a trailing `int` after `short`.

931 needed an `emit_if` fast-path. BCC constant-folds the
condition entirely: `if (1) { return 7; } return 0;` emits the
then-body inline (`mov ax, 7; jmp short @END`), then the
following statement (`xor ax, ax; jmp short @END`) with no
compare, no conditional jump, and no if-skip label between
them. The else-branch (if any) becomes dead code that BCC
emits anyway but never reaches. Implementation: at the top of
`emit_if`, run `try_const_eval(cond)`. If it folds, emit only
the relevant branch (then for non-zero, else for zero) and
skip the label-plan slot reservation entirely. The branch-skip
label that the conditional-jump path would emit is *not*
needed because there's no jump aimed at it — control simply
falls through to whatever comes next in the function body.

Same flavor as the existing `while (K)` fast-path further
down in this file: when a loop condition folds to a non-zero
constant, BCC elides the trampoline jump and the check label.
The `if (K)` shape is even simpler — no labels at all.

## do-while loops, while-global-cond

Fixtures `920` (do-while with accumulator: `do { s += i; i++;
} while (i < 5)`), `921` (basic do-while: `do { i++; } while
(i < 3)`), `922` (`int g; while (g) g = g - 1;` — while loop
with global zero-test condition).

All three already work end-to-end. Coverage notes:

- 920/921: do-while emits the body label at function entry,
  then the condition check at the bottom with a backward branch
  if true. No new IR — same shape as while-loop, just with the
  condition test moved to after the body.
- 922: while condition is a global zero-test. Reuses the
  existing `emit_zero_test` Ident-of-global arm (`cmp word ptr
  DGROUP:_g, 0`). The decrementing assignment `g = g - 1`
  lowers to `dec word ptr DGROUP:_g` (memory-direct INC/DEC
  peephole on int globals).

**Recorded findings from this batch (deferred):**

- **Function pointer assignment** (`int (*fp)(int) = f`): when
  RHS is a function symbol (not a local pointer), codegen
  panics with "unknown local in codegen: f". The assignment
  side needs an arm that recognizes the RHS as a function
  identifier and emits `mov word ptr <fp>, offset _f`.
- **Array-as-function-parameter** (`int f(int a[])`): parser
  fails at byte 11 with "expected `)`, got `[`" — the
  declarator grammar inside parameter lists doesn't yet
  accept the `T name[]` shorthand (must use `T *name`).
- **Array-decay-in-call-args** (`f(b)` where `b` is `int b[3]`
  and `f` expects `int *`): codegen emits `mov ax, word ptr
  DGROUP:_b` (value load) instead of `mov ax, offset DGROUP:_b`
  (address). The arg-prep path needs to detect array-typed
  args being passed to pointer params and emit the offset
  form.


## `goto` backward (loop reconstruction) — fixture `2340`

`label: ...; if (cond) goto label;` is the K&R way to write a loop.
BCC handles it via the same template it uses for any `if (cond) stmt`:
inverted comparison + skip-around + unconditional `jmp` back. The
backward `jmp` uses the `eb` short form (-128..+127 disp8).

```c
top:
  sum = sum + i;
  i = i + 1;
  if (i < 5) goto top;
```

```
; loop body inline (no fresh prologue per iteration — i,sum enregistered SI,DI)
8b c7 03 c6 8b f8       ; sum += i
8b c6 40 8b f0          ; i = i + 1
83 fe 05                ; cmp si, 5
7d 02                   ; jge +2      ← skip the jmp if !(i<5)
eb ee                   ; jmp -18     ← backward goto top (short form)
```

Both `i` and `sum` were enregistered (SI, DI) since they are the only
two locals — confirms the enregistration pool can carry across a
backward branch with no spill. The conditional was emitted as
`cmp/jge/jmp` (invert + skip) rather than `cmp/jl` direct — `goto`
shares the if-stmt lowering path.

## `goto` forward (skip code) — fixture `2341`

Forward `goto done;` to skip code uses the same `eb disp8` short form.

```c
  if (x > 0) goto done;
  x = 99;
done:
  return x;
```

```
be 05 00                ; mov si, 5 (x)
0b f6                   ; or si, si        ← test for !=0
7e 02                   ; jle +2           ← invert (x≤0): skip the goto
eb 03                   ; jmp +3           ← forward goto done
be 63 00                ; mov si, 99
; done:
8b c6 eb 00             ; return si
```

Two notes: (a) `x > 0` for a positive-likely value compiles to
`or reg, reg / jle` (zero-test peephole), not the literal
`cmp si, 0 / jg done`. (b) `goto done;` is again lowered as
`jcc skip; jmp done` — the `goto` keyword doesn't get a direct
conditional-jump shortcut.

## Nested ternary — sub-expressions re-evaluated, no CSE (fixture `2344`)

When the same ternary appears twice within a nested ternary expression,
BCC emits each occurrence in full — there is **no common-subexpression
elimination**.

```c
m = (a > b ? a : b) > c ? (a > b ? a : b) : c;
```

The inner `(a > b ? a : b)` is emitted twice — once as the LHS of the
outer comparison, and again as the if-true branch of the outer ternary:

```
; Inner ternary #1 — drives the outer compare
3b f7 7e 04 8b c6 eb 02 8b c7   ; (a>b ? a : b) → ax
3b c2                            ; cmp ax, c
7e 0c                            ; jle pick_c
; Inner ternary #2 — re-evaluated for the true branch
3b f7 7e 04 8b c6 eb 02 8b c7   ; (a>b ? a : b) → ax  (identical)
eb 02
; pick_c:
8b c2                            ; mov ax, c
89 46 fe                         ; m = ax
```

a, b, c are enregistered (SI, DI, DX) so the re-evaluation is cheap
(`cmp + 2 movs + 2 jmps` = 10 bytes), but it still doubles the
ternary's code size. Confirms BCC treats the ternary purely as a
syntactic expansion at codegen time — no caching of evaluated
sub-expressions across the `?` / `:` boundaries.

Also confirms the `dec sp / dec sp` small-frame heuristic for a single
2-byte local (`m`) — `4c 4c` is shorter than `83 ec 02`.

## `do { ... } while (--n);` — dec+jne combined (fixture `2361`)

When the loop condition is exactly the side-effect expression `--n`,
BCC fuses the decrement with the test into a single `dec / jne back`
pair. No separate `cmp` is emitted — the `dec` instruction sets flags
already.

```c
do {
  sum = sum + n;
} while (--n);
```

```
be 05 00                ; n = 5 (SI — enregistered)
33 ff                   ; sum = 0 (DI — enregistered)
; loop:
8b c7 03 c6 8b f8       ; sum += n
4e                      ; dec si        ← --n  (sets ZF)
75 f7                   ; jne loop (-9) ← test the dec result
```

So when both source pattern AND register live-state match, BCC
collapses to the minimum 3-byte loop tail: `dec / jne disp8`. This is
the optimal countdown-loop encoding. Contrast with explicit
`while (i < n)` which needs `cmp / jl` after each modification.

## Short-circuit `&&` in a loop condition — separate cmp+jcc per operand (fixture `2362`)

`while (*p == *q && *p)` is the strcmp-walk idiom. Each `&&` operand
gets its own `cmp + jcc` — no boolean materialization in this
position (no need to land in AX, since the only consumer is a branch).

```
; test (after the first-pass jmp-to-test):
8a 04                   ; mov al, [si]    ← *p
3a 05                   ; cmp al, [di]    ← *q  (byte cmp, no widening)
75 05                   ; jne exit        ← short-circuit: if *p != *q, exit
80 3c 00                ; cmp byte ptr [si], 0
75 f3                   ; jne loop_body   ← if *p != 0, continue
                        ; fallthrough to exit
```

Two notes:

1. `*p == *q` for char pointers uses **byte-width compare**
   (`8a 04 / 3a 05`) — no `cbw` widening to int. The comparison
   happens entirely in AL.
2. **`&&` short-circuit emits no `bool` value** in this branch-only
   context. The `cmp` sets flags directly for the `jcc` — no
   intermediate `mov ax, 0/1`. This is the same compare-as-branch
   path used by `if (a == b)`.

The final `(int)(*p - *q)` (after the loop exits) DOES widen via
`cbw`, since the result needs to land in AX as an int. So char-vs-int
contexts are decided per-expression, not globally.

The `while` loop again uses the jmp-to-test-first template: skip the
increment block on first iteration via `eb 02`.

## Explicit `return;` in `void` function — jmp to epilogue (fixture `2364`)

A bare `return;` statement inside a `void` function compiles to a
`jmp` to the function's epilogue. The early-out pattern `if (x < 0)
return;` follows the standard inverted-if template:

```c
if (x < 0) return;
x = x + 1;
```

```
8b 76 04                ; mov si, [bp+4] (x)
0b f6                   ; or si, si       ← test sign
7d 02                   ; jge +2          ← skip the early-return if x >= 0
eb 05                   ; jmp epilogue    ← early return
8b c6 40 8b f0          ; x = x + 1
5e 5d c3                ; epilogue: pop si; pop bp; ret
```

`0b f6 / 7d 02 / eb 05` — `or reg, reg / jge skip / jmp epilogue`.
Two short jumps with inverted condition, same as any `if (cond) goto
end;`. No special "return statement" opcode — early returns reuse
the `goto end` lowering. Confirms `return;` and `goto epilogue;` are
the same construct at codegen time.

## Multiple labels on one statement — same address (fixture `2373`)

`top: mid: end: x = x + 1;` — three labels stacked on the same
statement. All three resolve to the **same code address** (the start
of `x = x + 1`'s emitted code). No padding, no nop between them.

```c
goto mid;
top:
mid:
end:
  x = x + 1;
```

```
eb 00                   ; goto mid  ← jmp +0 (no-op since target is the next byte)
8b c6 40 8b f0          ; x = x + 1 (top: mid: end: all point here)
```

Two consequences:

1. Labels carry zero code-size cost — they're pure name-to-position
   mappings.
2. A `goto label` whose target is the immediately following
   instruction collapses to `eb 00` (jmp +0). BCC doesn't peephole
   this away.

Confirms labels are resolved purely positionally at codegen and
don't generate any per-label instruction.

## `if (cond) ;` — empty body still emits the test (fixture `2394`)

A bare semicolon as the if body is a syntactically valid empty
statement. BCC emits the full condition test and skip jump, with
the skip distance being zero (`jle +0` / similar):

```c
if (x > 0)
  ;
return x;
```

```
be 05 00                ; mov si, 5 (x)
0b f6                   ; or si, si       ← test
7e 00                   ; jle +0          ← skip target is the next instruction
8b c6                   ; mov ax, si      ← return x
```

The `7e 00` reads as "if x ≤ 0, skip 0 bytes" — a no-op control
flow. BCC keeps this skeleton because:

1. The condition's side effects must still execute. (`if (f()) ;`
   still calls `f()`.)
2. The codegen path doesn't have a "body is empty, elide the test"
   special case.

So **empty if bodies cost 4 bytes** here (`0b f6 / 7e 00` for an
or-zero-test + skip): the cmp/test plus a zero-displacement jump.
Consistent with control flow being structurally lowered without
peephole elision of unreachable paths.

## Multiple `return` statements — single epilogue, no flag reuse (fixture `2407`)

A function with multiple `return` paths converges all of them on a
single epilogue label via `jmp`:

```c
int classify(int x) {
  if (x < 0) return -1;
  if (x == 0) return 0;
  if (x < 10) return 1;
  return 2;
}
```

```
8b 76 04                ; mov si, x
0b f6                   ; or si, si       ← test x < 0
7d 05                   ; jge skip1       (if x >= 0)
b8 ff ff                ; ax = -1
eb 17                   ; jmp epilogue
; skip1:
0b f6                   ; or si, si       ← test x == 0  (REPEATED!)
75 04                   ; jne skip2
33 c0                   ; ax = 0
eb 0f                   ; jmp epilogue
; skip2:
83 fe 0a                ; cmp si, 10      ← test x < 10
7d 05                   ; jge skip3
b8 01 00                ; ax = 1
eb 05                   ; jmp epilogue
; skip3:
b8 02 00                ; ax = 2
eb 00                   ; jmp epilogue
; epilogue:
5e 5d c3                ; pop si; pop bp; ret
```

Two observations:

1. **Each return loads AX with the value, then `jmp` to the shared
   epilogue.** BCC never duplicates the epilogue per return — only
   one `pop si; pop bp; ret` exists in the function.
2. **No flag reuse**: the first test (`x < 0`) uses `or si, si /
   jge`. The second test (`x == 0`) uses the *same* `or si, si`
   again — even though the value in `si` is unchanged and the
   flags from the previous `or` are still valid. BCC re-emits the
   test from scratch.

So BCC's IR doesn't track live flags across statement boundaries.
Each comparison is lowered independently. A peephole that elides
the second `or si, si` when the value is provably unchanged would
save 2 bytes per redundant test — but BCC doesn't implement it.

## `while (--n) ;` — empty body, jmp-to-test still emitted (fixture `2408`)

```
be 05 00                ; n = 5
eb 00                   ; jmp test      ← still emitted even with empty body
                        ; (loop body is empty)
                        ; test:
4e                      ; dec si        (--n)
75 fd                   ; jne loop_top (-3)
```

The unconditional jmp to the test is the standard while-template
preamble — emitted regardless of body content. For empty bodies it
collapses to `eb 00` (jmp +0). Same pattern as the empty-if-body
case.

So the loop costs **5 bytes** for the while skeleton (`eb 00 / dec /
jne disp8`) even when the body is empty. Confirms: control-flow
templates are emitted structurally, not optimized away on empty
content.

## `(*pfn)(arg)` byte-identical to `pfn(arg)` (fixture `2414`)

Both forms of indirect function call produce **identical OBJ**:

```c
pfn = square;
return (*pfn)(7);    // explicit dereference
// vs.
return pfn(7);       // implicit (function-to-pointer + auto-deref)
```

```
b8 07 00                ; ax = 7
50                      ; push 7
ff 56 fe                ; call near [bp-2]
59                      ; pop cx
```

C90 defines `f` and `(*f)` as equivalent when `f` is a function or
function pointer (functions decay to pointers; dereferencing a
function-pointer produces a function value that immediately decays
back to a pointer for the call). BCC implements this equivalence
at parse time — the AST collapses both spellings to the same call
node before codegen sees them.

So the `(*pfn)(...)` idiom (common in K&R code) carries zero codegen
cost — the leading `*` is purely syntactic noise from the compiler's
perspective.

## `continue` — jumps to test, not loop top (fixtures `2441`, `2446`)

`continue` inside `while`/`do-while`/`for` jumps to the **test** (or
the `update` clause for `for`), NOT back to the body-start. This
means side effects in the loop's increment expression still execute
on a `continue`.

For `while (cond) { body; continue; rest }`:

```
                        ; loop_top:
8b c6 40 8b f0          ; ... loop body (which has i = i + 1) ...
83 fe 05                ; cmp si, 5      ← compare for continue cond
75 02                   ; jne skip_continue
eb 06                   ; jmp test       ← continue: jump to test
                        ; skip_continue:
8b c7 03 c6 8b f8       ; rest of body (sum += i)
                        ; test (also continue target):
83 fe 0a                ; cmp si, 10
7c e9                   ; jl loop_top
```

For `do { body; continue; rest } while (cond)`, same pattern — the
`continue` target is the test at the bottom, not the body top.

So `continue` is structurally a **`jmp` forward to the test**, which
allows any pending increment work (in for loops) or test to run.
The control-flow lowering treats `continue` exactly like
`if (cond) ; else { rest }` plus an unconditional jmp.

## `&&` chain of three — three serial cmp/je to one false target

Fixture `2505-and-chain-three-obj`:

```c
if (a && b && c) return 7;
return 0;
```

(with `a = b = c = 1` initializers above)

```
55 8b ec 83 ec 06     prologue + sub sp, 6 (3 locals)
c7 46 fe 01 00        a = 1
c7 46 fc 01 00        b = 1
c7 46 fa 01 00        c = 1
83 7e fe 00           cmp word [bp-2], 0    ; test a
74 11                 je +17                ; → false path
83 7e fc 00           cmp word [bp-4], 0    ; test b
74 0b                 je +11                ; → false path
83 7e fa 00           cmp word [bp-6], 0    ; test c
74 05                 je +5                 ; → false path
b8 07 00              mov ax, 7             ; TRUE: result
eb 04                 jmp +4                ; → epilogue
33 c0                 xor ax, ax            ; FALSE: result
eb 00 8b e5 5d c3     epilogue
```

Findings:
- Three-way `&&` compiles as **three serial `cmp word [mem], 0` +
  `je false-path`**, sharing the same false-path target.
- Each `je` uses **disp8 (short jump)** — disps shrink (0x11, 0x0b,
  0x05) because each subsequent test is physically closer to the
  false-target.
- The TRUE path emits the result-into-ax + `jmp` to a common
  epilogue point; the FALSE path emits `xor ax, ax` and falls
  through. So the merge point is the single epilogue, NOT a per-
  branch `return`.
- 6-byte local reserve uses `sub sp, 6` (consistent with earlier
  finding: ≥3B uses sub).
- This is the canonical "short-circuit chain" shape: N operands →
  N cmp/je pairs targeting one common false label, then one true-
  path emit, then merge.


## Nested ternary — cmp/jne cascade ending in else-of-else value

Fixture `2508-nested-ternary-obj`:

```c
int x = 2;
return x == 1 ? 10 : x == 2 ? 20 : 30;
```

```
55 8b ec 56                prologue + push si
be 02 00                   mov si, 2                ; x in si
83 fe 01                   cmp si, 1
75 05                      jne +5
b8 0a 00                   mov ax, 10
eb 0d                      jmp +13 (epi)
83 fe 02                   cmp si, 2
75 05                      jne +5
b8 14 00                   mov ax, 20
eb 03                      jmp +3 (epi)
b8 1e 00                   mov ax, 30
eb 00 5e 5d c3             epilogue
```

Findings:
- Right-associative ternary `a ? b : c ? d : e` compiles as two
  sequential `cmp/jne + mov ax, K; jmp end` blocks, with the final
  else (`mov ax, 30`) falling through naturally.
- Every "then" arm emits its result and `jmp` to the **same merge
  point** (the epilogue). No per-branch return — single epilogue
  serves all paths.
- Source-order tests preserved: x==1 is tested before x==2.
- All conditional jumps are disp8 (short forward). The jmp to
  epi shrinks by 10 each iteration (0x0d → 0x03 → fallthrough).
- This is structurally identical to an if/else-if/else cascade —
  ternary and if-else are interchangeable at this codegen layer
  when they produce the same value-flow.


## `do { } while (i > 0)` — full register promotion, AX as accumulator

Fixture `2510-do-while-real-cond-obj`:

```c
int i = 5, sum = 0;
do {
  sum = sum + i;
  i = i - 1;
} while (i > 0);
return sum;
```

```
55 8b ec 56 57             prologue + push si, di
be 05 00                   mov si, 5            ; i in si
33 ff                      xor di, di           ; sum in di
                           ; ---- LOOP TOP ----
8b c7                      mov ax, di           ; ax = sum
03 c6                      add ax, si           ; ax += i
8b f8                      mov di, ax           ; sum = ax
8b c6                      mov ax, si           ; ax = i
48                         dec ax               ; ax--
8b f0                      mov si, ax           ; i = ax
0b f6                      or si, si            ; flags from i
7f f1                      jg -15               ; goto LOOP TOP
8b c7                      mov ax, di           ; return sum
eb 00 5f 5e 5d c3          epilogue
```

Findings:
- **All locals promoted to registers**: i → si, sum → di. The
  function uses zero stack slots beyond the saved bp/si/di.
- **AX is the universal accumulator**: every expression result
  passes through ax even when the more compact form would be
  `add di, si` or `dec si`. So `sum += i` becomes 3 instructions
  (load to ax, add, store back), and `i--` becomes 3 instructions
  (load to ax, dec, store back). This is a consistent BCC quirk
  worth catching in the codegen IR: it's the **"expressions always
  flow through ax"** invariant.
- The `i > 0` test uses **`or reg, reg` (1 byte)** to set flags
  from si itself, then `jg` for signed-greater-than. No `cmp`.
- Backward branch is `7f f1` = `jg -15` — disp8 reaches back to
  the LOOP TOP. Loop body is well under 128 bytes.
- The post-condition design of `do/while` means the LOOP TOP label
  is *exactly* where the body starts — no entry-condition pre-check
  like `while(){}` has.


## `for(;;) { ...; if (cond) break; }` — break is a jmp past a jmp

Fixture `2516-for-infinite-obj`:

```c
int g;
for (;;) {
  g = g + 1;
  if (g > 100) break;
}
return g;
```

```
55 8b ec                       prologue
                               ; ---- LOOP TOP ----
a1 00 00                       mov ax, [_g]              ; FIXUPP _g
40                             inc ax
a3 00 00                       mov [_g], ax              ; FIXUPP _g
83 3e 00 00 64                 cmp word [_g], 100        ; FIXUPP _g
7e 02                          jle +2 → SKIP-BREAK
eb 02                          jmp +2 → after-loop
                               ; ---- SKIP-BREAK ----
eb ee                          jmp -18 → LOOP TOP
                               ; ---- after-loop ----
a1 00 00                       mov ax, [_g]              ; return g
eb 00 5d c3                    epilogue
```

Findings:
- `for(;;)` has zero init/cond/inc, so the **LOOP TOP is the body's
  first instruction** — no entry stub.
- `if (g > 100) break` compiles to a TWO-jump sequence:
  - `cmp ... ; jle SKIP-BREAK` — i.e., the conditional CONTINUES the
    loop when condition is FALSE.
  - Then `jmp after-loop` — the actual break.
  This double-jump shape (jle past a jmp) is **literally** the IR
  `if (cond) { break; }` → "if (!cond) goto skip; break-jmp".
- The conditional uses **`cmp word [_g], 100`** (direct mem operand)
  for the compare — no load to register first. This is the
  `83 3e disp16 imm8` form (sign-extended imm8).
- The unconditional back-edge is `eb ee` (= jmp -18), reaching back
  to the LOOP TOP — 18-byte loop body fits in disp8.
- `++g` on a global directly uses `mov ax, [_g]; inc ax; mov [_g], ax`
  — the AX accumulator pattern persists even for globals (no
  `inc word [_g]` peephole, which would be 4 bytes vs the 7 used).


## `goto top;` — same bytes as a `while`-back-edge

Fixture `2517-goto-label-obj`:

```c
int i = 0;
top:
  i = i + 1;
  if (i < 3) goto top;
return i;
```

```
55 8b ec 56                    prologue + push si
33 f6                          xor si, si       ; i in si
                               ; ---- top: ----
8b c6                          mov ax, si       ; i++ via AX
40                             inc ax
8b f0                          mov si, ax
83 fe 03                       cmp si, 3
7d 02                          jge +2 → skip-goto
eb f4                          jmp -12 → top
                               ; ---- skip-goto ----
8b c6                          mov ax, si       ; return i
eb 00 5e 5d c3                 epilogue
```

Findings:
- `goto top` compiles to a plain `jmp disp8` to the label. The
  conditional-goto shape `if (i < 3) goto top` is identical to a
  `while (i < 3)` back-edge: `cmp; jge past-jmp; jmp -N`. So
  **goto and structured back-edges produce the same bytes**.
- The AX-accumulator pattern is unbroken: `i++` is 3 instructions
  (`mov ax, si; inc ax; mov si, ax`) NOT the single `inc si`.
  Consistent with `2510` and `2516` — this is the canonical
  expression-evaluation shape, not an edge case.


## `||` short-circuit — `jnz <true>` for all but last, `jz <false>` for last

Fixture `2534-or-shortcircuit-obj`:

```c
if (a || b) return 7;
return 0;
```

```
55 8b ec 83 ec 04              prologue + 4B locals
c7 46 fe 00 00                 a = 0
c7 46 fc 01 00                 b = 1
83 7e fe 00                    cmp word [bp-2], 0    ; test a
75 06                          jnz +6 → TRUE         ; a≠0 → short-circuit TRUE
83 7e fc 00                    cmp word [bp-4], 0    ; test b
74 05                          jz +5  → FALSE        ; b==0 → FALSE
                               ; TRUE-PATH (a≠0 fallthrough, or b≠0 here):
b8 07 00                       mov ax, 7
eb 04                          jmp epi
                               ; FALSE-PATH:
33 c0                          xor ax, ax
eb 00 8b e5 5d c3              epilogue
```

Findings:
- `||` is structurally the dual of `&&`:
  - **All but the LAST operand**: `cmp; jnz <TRUE-PATH>` —
    short-circuit to TRUE if non-zero.
  - **The LAST operand**: `cmp; jz <FALSE-PATH>` — short-circuit
    to FALSE if zero, fall through to TRUE.
- Merge structure is identical to `&&`: single TRUE-path and
  single FALSE-path joining at the epilogue.
- All conditional jumps are short (disp8).
- Pattern flips between `je`/`jz` (AND) and `jne`/`jnz` (OR) —
  one bit toggled in the jump opcode (`74` ↔ `75`).
- Both AND and OR fall through (no jump) into the TRUE-path
  when the cascade completes successfully — the `jmp +4` to epi
  is from the explicit `return 7` body.


## `while (*p != 0)` — test-at-bottom shape, direct `cmp byte [si], 0`

Fixture `2561-while-sentinel-obj`:

```c
char buf[5] = "hi";
int main(void) {
  char *p = buf;
  int n = 0;
  while (*p != 0) {
    n = n + 1;
    p = p + 1;
  }
  return n;
}
```

```
55 8b ec 56 57                 prologue + push si, di
be 00 00                       mov si, _buf (FIXUPP)    ; p in si
33 ff                          xor di, di               ; n in di
eb 0a                          jmp +10 → COND
                               ; ---- LOOP-BODY ----
8b c7 40 8b f8                 n = n+1 (AX-accum)
8b c6 40 8b f0                 p = p+1 (AX-accum)
                               ; ---- COND ----
80 3c 00                       cmp byte [si], 0         ; direct mem-to-imm8
75 f1                          jnz -15 → BODY
8b c7                          mov ax, di               ; return n
eb 00 5f 5e 5d c3              epilogue
```

Findings:
- **`while`** uses **test-at-bottom** structure: initial `jmp +N → COND`,
  then body, then condition with backward `jnz/jne` to body start.
  Compare to `do/while` (no initial jump) and `for(;;)` (no entry stub).
- The sentinel check `*p != 0` emits as **`cmp byte ptr [si], 0`**
  (`80 3c 00`, opcode `80` /7 byte-cmp with imm8, ModR/M `3c` = mod 00
  r/m 100 = `[si]`, then `00` is the imm8). 3 bytes total, NO load
  to register first.
- `_buf` initialized with `"hi"` produces 5-byte _DATA segment with
  `68 69 00 00 00` — char[] partial init **zero-fills the rest**.
- Note: `n = n + 1` uses the AX-accumulator pattern (3 instr) — NOT
  `inc reg`. So **the source FORM matters**: `n++` would emit `inc
  di` directly, but `n = n + 1` always goes through AX.


## `for(init; cond; inc)` — init outside, increment APPENDED to body

Fixture `2568-for-full-form-obj`:

```c
for (i = 0; i < 5; i = i + 1) {
  s = s + i;
}
```

```
33 ff 33 f6                    init: di=0 (s), si=0 (i)
eb 0b                          jmp → COND
                               ; ---- BODY (includes the for's INC clause) ----
8b c7 03 c6 8b f8              s = s + i  (AX-acc)
8b c6 40 8b f0                 i = i + 1  (AX-acc, the for-INC clause)
                               ; ---- COND ----
83 fe 05                       cmp si, 5
7c f0                          jl -16 → BODY (signed branch)
8b c7                          return s
```

Findings:
- `for(init; cond; inc) { body }` desugars to:
  ```
  init;
  goto COND;
  TOP: body; inc;
  COND: if (cond) goto TOP;
  ```
- The **for-loop's INCREMENT clause is appended to the body** —
  not a separate region. So from a codegen perspective, `for` is
  identical to:
  ```
  init; while (cond) { body; inc; }
  ```
- Both `s` and `i` get register promotion to di/si even though
  `s = 0` is OUTSIDE the for in the source.
- Conditional uses `jl` (signed less-than) — matches signed `int i`.
- Same "test-at-bottom" frame as while (`2561`).


## `continue` in `for(;;)` — jumps to INCREMENT clause, not COND-TEST

Fixture `2578-continue-in-loop-obj`:

```c
for (i = 0; i < 5; i = i + 1) {
  if (i == 2) continue;
  s = s + i;
}
```

```
33 ff 33 f6                    init (di=s=0, si=i=0)
eb 12                          jmp → COND
                               ; --- BODY ---
83 fe 02                       cmp si, 2
75 02                          jne → SKIP-CONTINUE
eb 06                          jmp → INC          ; CONTINUE = jmp to INC
                               ; SKIP-CONTINUE:
8b c7 03 c6 8b f8              s = s + i
                               ; INC (continue target):
8b c6 40 8b f0                 i = i + 1
                               ; COND:
83 fe 05                       cmp si, 5
7c e9                          jl → BODY
```

Findings:
- **`continue` jumps to the for-loop's INCREMENT clause**, not the
  condition test. So the increment STILL runs before the next
  condition check. This matches C semantics — `for(i=0;i<5;i++)`
  with continue still advances i.
- `if (cond) continue` compiles to `cmp; jne SKIP; jmp INC` — same
  double-jump shape as `break` (`2516`), but jumping to a DIFFERENT
  target.
- The increment clause label sits between the body and the cond-test.
  So for a `while(cond) { ... continue; }`, continue would jump
  directly to the cond-test (no inc clause).
- This means the parser must emit a distinct "continue target" label
  per loop level (the for-inc clause), which is structurally
  different from the cond-test label.


## `break` in `while` — same shape as in `for`, just jmp to after-loop

Fixture `2583-break-in-while-obj`:

```c
while (i < 10) {
  if (i == 4) break;
  i = i + 1;
}
```

```
33 f6                          xor si, si        ; i = 0
eb 0c                          jmp → COND
                               ; --- BODY ---
83 fe 04                       cmp si, 4
75 02                          jne → SKIP-BREAK
eb 0a                          jmp → after-loop  (BREAK)
                               ; SKIP-BREAK:
8b c6 40 8b f0                 i = i + 1
                               ; --- COND ---
83 fe 0a                       cmp si, 10
7c ef                          jl → BODY (signed)
                               ; after-loop:
8b c6                          mov ax, i
```

Findings:
- `break` in `while` uses the **same double-jump pattern** as
  `break` in `for` (`2516`): `cmp; jne SKIP; jmp after-loop`.
- The target label "after-loop" is the same regardless of loop
  kind; the only difference between loops is where `continue`
  targets:
  - `while`/`do`: continue → COND
  - `for`:        continue → INCREMENT clause (`2578`)
- So **break shape is loop-kind-independent**, **continue shape
  varies by loop kind**.


## `if (...) ... else if (...) ... else` — extra unreachable jmp per branch

Fixture `2587-if-else-chain-obj`:

```c
if      (x == 1) return 10;
else if (x == 2) return 20;
else if (x == 3) return 30;
else             return 99;
```

```
55 8b ec 56                    prologue + push si
be 03 00                       mov si, 3
                               ; --- if x==1 ---
83 fe 01                       cmp si, 1
75 07                          jne → else1
b8 0a 00                       mov ax, 10
eb 1f                          jmp → epi
eb 1d                          jmp → epi    (UNREACHABLE!)
                               ; --- else1: if x==2 ---
83 fe 02                       cmp si, 2
75 07                          jne → else2
b8 14 00                       mov ax, 20
eb 13                          jmp → epi
eb 11                          jmp → epi    (UNREACHABLE!)
                               ; --- else2: if x==3 ---
83 fe 03                       cmp si, 3
75 07                          jne → else3
b8 1e 00                       mov ax, 30
eb 07                          jmp → epi
eb 05                          jmp → epi    (UNREACHABLE!)
                               ; --- else3 ---
b8 63 00                       mov ax, 99
eb 00 5e 5d c3                 epilogue
```

Findings:
- Each then-branch with an explicit `return` is followed by a
  **second, unreachable `jmp` to epi**. BCC structurally emits:
  ```
  cmp; jne ELSE; THEN_BODY; jmp EPI;
                            jmp END_IF;
  ELSE: ...
  END_IF: ...
  ```
  The "jmp EPI" comes from the return; the "jmp END_IF" would
  normally bridge over the else-branch. Both are emitted even
  when the first is final.
- This pattern **wastes 2 bytes per if-else with a returning then**.
  No dead-code elimination.
- The dead jmp's disp also shrinks meaningfully along the chain
  (0x1d, 0x11, 0x05) — BCC computes disps with full structural
  awareness, just doesn't prune them.
- **Source structure preserved 1:1 in OBJ**: if-else cascades
  produce nested branch blocks with predictable shape, which
  makes our reimpl's job easier — we just emit the same shape.


## `while (*p)` is byte-identical to `while (*p != 0)`

Fixture `2605-while-deref-incr-obj`:

```c
while (*p) {
  p = p + 1;
  n = n + 1;
}
```

```
55 8b ec 56 57 be 00 00         prologue + push si, di + load p
33 ff                            xor di, di
eb 0a                            jmp → COND
                                 ; --- BODY ---
8b c6 40 8b f0                   p = p + 1 (AX-acc via si)
8b c7 40 8b f8                   n = n + 1 (AX-acc via di)
                                 ; --- COND ---
80 3c 00                         cmp byte [si], 0
75 f1                            jnz → BODY
8b c7                            mov ax, n
eb 00 5f 5e 5d c3                epilogue
```

Findings:
- `while (cond)` for any non-zero-test `cond` uses **byte-identical
  shape** whether written as `while (*p)` or `while (*p != 0)`.
- The implicit "is non-zero" test compiles to the SAME
  `cmp byte [si], 0; jnz BODY` sequence as the explicit form.
- So our parser can canonicalize `while (X)` → `while (X != 0)`
  with no byte impact.
- Same applies to `if (X)` vs `if (X != 0)`.
- The "memory operand for byte compare" is `80 3c 00` (3 bytes):
  opcode `80 /7` = cmp byte r/m with imm8, ModR/M `3c` = mod 00
  r/m 100 = `[si]`, then imm8 = 0.


## `if (a && b) ... else ...` — `&&` chain joins at ELSE, then-jmp dead

Fixture `2612-and-with-else-obj`:

```c
if (a && b) return 10;
else return 20;
```

```
83 7e fe 00                    cmp word [bp-2], 0   ; test a
74 0d                          je → ELSE
83 7e fc 00                    cmp word [bp-4], 0   ; test b
74 07                          je → ELSE
                               ; THEN:
b8 0a 00                       mov ax, 10
eb 07                          jmp → epi
eb 05                          jmp → epi   (UNREACHABLE dead)
                               ; ELSE:
b8 14 00                       mov ax, 20
eb 00 8b e5 5d c3              epilogue
```

Findings:
- `if (a && b)` uses the canonical `&&` chain shape from `2505`:
  N cmp/je instructions, all converging on a single FALSE label
  (here ELSE, since the explicit `else` is present).
- The dead "jmp end_if" emitted between THEN and ELSE is the same
  as the if-else-chain pattern from `2587`. **2 wasted bytes per
  if-then-else block.**
- Source-form preservation: the explicit `else` makes the FALSE
  target be the else-body's address, not the post-block address.


## Mixed `a && (b || c)` — single flat chain with mixed je/jne

Fixture `2615-mixed-and-or-obj`:

```c
if (a && (b || c)) return 7;
return 0;
```

```
83 7e fe 00 74 11              cmp a; je → FALSE         (AND-context: 0 → false)
83 7e fc 00 75 06              cmp b; jne → TRUE         (OR-context: nz → true)
83 7e fa 00 74 05              cmp c; je → FALSE         (OR-context last: 0 → false)
                               ; TRUE:
b8 07 00 eb 04                 ax = 7; jmp epi
                               ; FALSE:
33 c0 eb 00                    xor ax, ax
8b e5 5d c3                    epilogue
```

Findings:
- Nested boolean trees flatten to a **single chain of cmp +
  conditional-jump** instructions, with the jump TYPE (`je` vs
  `jne`) chosen by the operand's logical context:
  - `je → FALSE` for AND-context operands (and the LAST operand of
    an OR-chain) — falsey value short-circuits.
  - `jne → TRUE` for OR-context operands (all but the last) —
    truthy value short-circuits.
- The merge structure stays simple: ONE common TRUE label and ONE
  common FALSE label, both at the end before the epilogue. All
  branches converge.
- Precedence and parens are baked into the operand-context
  determination at parse time; codegen sees a flat sequence with
  per-position labels.
- This generalizes to any depth of nested `&&`/`||`.


## Long if-body — disp8 `jne` reaches up to +127 byte forward

Fixture `2622-long-jmp-disp16-obj` — 24 successive `x = x + 1`
statements inside an if-then. Each `x = x + 1` is 5 bytes (`8b c6
40 8b f0` = mov ax,si; inc ax; mov si, ax — AX-acc form). Body
total ≈ 120 bytes + trailing `eb 04` (skip-else).

The `or si, si; jne ELSE` at the top of the if uses **`75 7c`** =
`jne +124`, which is still within the disp8 range (-128..+127).

Findings:
- Forward `jne` with disp = 124 fits in the 2-byte `75 disp8`
  form. BCC uses disp8 whenever possible.
- To force the disp16 form (`0f 85 disp16`, 4 bytes — the long
  conditional jump), the displacement would need to exceed +127.
  Bodies under ~120 bytes generally stay in disp8.
- Each `x = x + 1` is **5 bytes via AX-accumulator** (`mov ax, si;
  inc ax; mov si, ax`) — confirmed once more that BCC routes
  arith assignments through AX even for register-promoted locals.
- The body of 24 increments shows the AX-acc pattern's verbosity:
  the equivalent `for (...) x++;` would be far shorter, since
  `x++` uses `inc reg` directly (1 byte per inc).


## Long forward conditional jump — `j<inv> +3; jmp disp16` trampoline

Fixture `2627-disp16-jne-obj` — 32 successive `x = x + 1` in an
if-then body. Forward displacement to ELSE > 127.

```
33 f6                          xor si, si        ; x = 0
0b f6                          or si, si         ; test x
74 03                          je +3 → SKIP-LONG-JMP
e9 a4 00                       jmp +164 → ELSE
                               ; SKIP-LONG-JMP / THEN body:
... 32× (mov ax, si; inc ax; mov si, ax) = 160 bytes
8b c6 eb 04                    ax = x ; jmp epi
33 c0 eb 00                    ELSE: ax = 0
5e 5d c3                       epilogue
```

Findings:
- **8086 has no conditional disp16 jumps** (those are 80386+).
  When BCC needs to jump >127 bytes on a condition, it INVERTS
  the condition and emits a 2-byte short jump over a 3-byte
  `jmp disp16`. Total 5 bytes:
  - `j<inverted-cond> +3` (skip the long jmp = take the conditional path)
  - `jmp disp16` (target = original conditional target)
- So `if (x == 0)` with a 160-byte then-body compiles as:
  ```
  or si, si
  je +3        ; if x==0, skip the long jmp (fall into then-body)
  jmp ELSE     ; else, take the long jmp
  THEN body
  ```
- This is **5 bytes total** vs 2 bytes for a regular short `jne`.
  BCC pays this cost only when displacement exceeds ±127.
- Pattern generalizes: every `j<cond>` family member gets the
  same trampoline treatment when needed.
- `e9 disp16` = unconditional jmp near (3 bytes). The disp16 is
  signed relative to the byte after the jmp.

