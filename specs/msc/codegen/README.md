# MSC codegen — per-topic catalog

How source constructs lower to x86 in our `crates/msc` implementation,
following what we've observed `CL.EXE /c /AS` actually produces.
Each note links the fixtures that pin the behavior.

## Topic index

- [`PROLOGUE.md`](PROLOGUE.md) — frame shapes (none / bp-only /
  with-slide), `__chkstk` argument convention, zero-fill of AX before
  the call. Fixtures 4075, 4076, 4099.
- [`RETURN.md`](RETURN.md) — return-value emission, including the
  `2b c0` (sub ax, ax) quirk distinct from `33 c0`. Fixtures 4077,
  4099.
- [`ARITHMETIC.md`](ARITHMETIC.md) — int arithmetic, constant folding,
  strength-reductions (×2 → shl, ×3 → shift-add). Fixtures 4082–4089.
- [`CONTROL_FLOW.md`](CONTROL_FLOW.md) — if/else, while, do-while, for;
  even-byte loop-top alignment via NOP pad; const-cond elision.
  Fixtures 4090–4098.
- [`CALLS.md`](CALLS.md) — cdecl push order, `add sp, N` cleanup, intra-TU
  vs external call FIXUPs, argument-shape emission. Fixtures 4099–4103.
- [`GLOBALS.md`](GLOBALS.md) — init globals → PUBDEF, tentative globals
  → COMDEF, constant propagation across straight-line statements.
  Fixtures 4104–4106.
- [`POINTERS_ARRAYS.md`](POINTERS_ARRAYS.md) — int / char arrays,
  pointer init, deref / index, address-of, byte vs word load shapes.
  Fixtures 4107–4125.

## Quick reference: instruction shapes we emit

| Source                              | Emitted bytes                                    |
|-------------------------------------|--------------------------------------------------|
| Function prologue (with locals)     | `55 8b ec b8 size 00 e8 disp disp`               |
| Function prologue (params only)     | `55 8b ec 33 c0 e8 disp disp`                    |
| Function prologue (no locals/params)| `33 c0 e8 disp disp`                             |
| `return 0;`                         | `2b c0 c3`                                       |
| `return K;` (K ≠ 0)                 | `b8 K K c3`                                      |
| `int g = K;` access                 | `a1 disp disp` + GlobalAddr FIXUP                |
| `int g = K;` write of literal       | `c7 06 disp disp imm imm` + GlobalAddr FIXUP     |
| `a[K]` int read (const)             | `a1 byte_off byte_off` + FIXUP                   |
| `a[K] = V;` int store (const)       | `c7 06 byte_off byte_off imm imm` + FIXUP        |
| `s[K]` char read (const)            | `a0 byte_off byte_off 98` + FIXUP                |
| `s[K] = V;` char store (const)      | `c6 06 byte_off byte_off imm` + FIXUP            |
| `*p` (char *) read                  | `8b 1e 00 00` + FIXUP `8a 07 98`                 |
| `*p` (int *param) read              | `8b 5e disp 8b 07`                               |
| `*p = K;` through ptr global        | `8b 1e 00 00` + FIXUP `c7 07 imm imm`            |
| `p[K]` char-ptr read (const)        | `8b 1e 00 00` + FIXUP `8a 47 disp 98`            |
| `p[K] = V;` char-ptr store          | `8b 1e 00 00` + FIXUP `c6 47 disp imm`           |
| `&g` as arg                         | `b8 00 00 50` + GlobalAddr FIXUP                 |
| Push int literal arg                | `b8 K K 50`                                      |
| Push local arg                      | `ff 76 disp`                                     |
| Push param arg                      | `ff 76 disp`                                     |
| `add sp, N` cleanup                 | `83 c4 N`                                        |
| Variable index into array `a[i]`    | `8b 5e disp d1 e3 8b 87 00 00` + FIXUP           |
| `int a[N];` local array read `a[K]` | `8b 46 disp` (disp = local_disp + K*2)           |
| `int a[N];` local array write `a[K] = V` | `c7 46 disp lo hi`                          |
| `char s[N];` local array read `s[K]`| `8a 46 disp 98` (disp = local_disp + K)          |
| `char s[N];` local array write `s[K] = V` | `c6 46 disp imm8`                          |
| `g[K]` + `g[J]` chain (int globals) | `a1 K*2; 03 06 J*2 lo hi` + FIXUPs              |
| `a + b` (both locals/params)        | `8b 46 a_disp; 03 46 b_disp`                     |
| `n * fact(n-1)` (call-result swap)  | `8b 5e n_disp; 48; 50; e8 disp; 83 c4 02; f7 6e n_disp` |
| `cmp <local>, <param>`              | `8b 46 l_disp; 3b 46 p_disp`                     |
| `cmp <local>, <local>`              | `8b 46 l1_disp; 3b 46 l2_disp`                   |

## Frame layout (local declarations)

Each local pads up to an even byte count:

| Decl                | Bytes consumed |
|---------------------|----------------|
| `char c;`           | 2              |
| `int x;`            | 2              |
| `int *p;` (any ptr) | 2              |
| `int a[3];`         | 6              |
| `char s[3];`        | 4 (3 + 1 pad)  |
| `int a[10];`        | 20             |

Source-order allocation: the first declarator gets the shallowest slot (closest to BP). Within an array, element 0 sits at the deepest disp.

See [`msc-frame-layout`](../../../~/.claude/.../msc_frame_layout.md)
for the disp-computation formula and the BP-relative emission rules.

## Const-prop driven shapes

MSC's optimizer is aggressive. Many fixtures pass because we match its
const-fold output:

- `int x = 5; return x + 1;` → `b8 06 00 c3` (full fold).
- `int g = 5; g = 7; return g;` → `c7 06 addr 07 00; b8 07 00 c3`.
- `int a[3]; a[1] = 10; return a[1];` → `c7 46 fc 0a 00; b8 0a 00 c3`.
- `int x = 1; switch (x) { case 1: r = 20; break; ... }` → emits only the `r = 20` store.
- `int r = x <= y;` with x, y both init → `c7 46 disp imm` with imm = 0 or 1.

See [`msc-const-prop-scope`](../../../~/.claude/.../msc_const_prop_scope.md)
for the propagation rules and [`msc-switch-lowering`](../../../~/.claude/.../msc_switch_lowering.md)
for the foldable-switch rewrite.
