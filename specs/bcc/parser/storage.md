## static local variable — _DATA storage, no PUBDEF, pre/post inc via mem-direct

Fixture `3311-static-local-obj`:

```c
int next(void) { static int n = 0; return ++n; }
```

- `static int n = 0` placed in _DATA segment with LEDATA (4-byte zero init record).
- No PUBDEF emitted (static = file-scope private).
- Internal LIDATA/LEDATA record initializes the cell.

Body:
```
ff 06 00 00 [FIXUPP _n]        inc word [n]
a1 00 00 [FIXUPP _n]           mov ax, [n]
```

Findings:
- Pre-increment on static int: `inc mem` (4B with FIXUPP), then load.
- Compare to static int with `= 0`: still goes in _DATA, not _BSS. Only uninitialized `static int n;` lands in _BSS.


## Array initializer `int arr[5] = {1,2,3,4,5}` — single LEDATA in _DATA

Fixture `3335-arr-init-full-obj`:

- `_arr` placed in _DATA segment.
- _DATA size = 10 bytes (5 ints).
- Single LEDATA record at offset 0 with the 10 bytes: `01 00 02 00 03 00 04 00 05 00`.

Body:
```
a1 00 00 [FIXUPP _arr]         mov ax, [arr+0]
03 06 08 00 [FIXUPP _arr]      add ax, [arr+8]
```

Findings:
- Initializer values written into _DATA via single LEDATA.
- arr[N] reads via direct mem-load with disp = N * sizeof(elem).

## Partial array init `int arr[5] = {1,2}` — full LEDATA with explicit zeros

Fixture `3336-arr-init-partial-obj`:

- _DATA size = 10 bytes (full array size).
- LEDATA bytes: `01 00 02 00 00 00 00 00 00 00` — explicit trailing zeros.

Findings:
- Partial init does NOT save bytes by tail-truncating + relying on _BSS for zeros.
- Whole array lives in _DATA with zeros explicitly written.

## `char s[N] = "string"` — literal stored directly in _DATA

Fixture `3337-char-arr-strinit-obj`:

```c
char s[6] = "hello";
```

- _DATA size = 6 bytes.
- LEDATA: `68 65 6c 6c 6f 00` — "hello\0".

Body:
```
a0 00 00 [FIXUPP _s]           mov al, [s]     (single byte load)
```

Findings:
- Initializer "hello" placed directly into _DATA at `_s` offset.
- Includes the implicit \0 terminator (since N=6 matches string length+1).


## struct init `{3, 4}` — flat LEDATA in declaration order

Fixture `3341-struct-init-obj`:

```c
struct Pt { int x; int y; } p = {3, 4};
```

- _DATA size = 4 bytes.
- LEDATA bytes: `03 00 04 00`.

Body:
```
a1 00 00 [FIXUPP _p]           mov ax, [p.x]
03 06 02 00 [FIXUPP _p]        add ax, [p.y]
```

Findings:
- Members written in declaration order: x first at offset 0, y next at offset 2.
- 4-byte LEDATA, no padding.

## Nested struct init — flattens to single LEDATA

Fixture `3342-struct-nested-init-obj`:

```c
struct { int a; struct { int b, c; } inner; } s = {1, {2, 3}};
```

- _DATA size = 6 bytes (3 ints).
- LEDATA: `01 00 02 00 03 00` — flat layout, inner members inline.

Body:
```
a1 00 00 [FIXUPP _s]           mov ax, [s+0]    (s.a)
03 06 02 00 [FIXUPP _s]        add ax, [s+2]    (s.inner.b)
03 06 04 00 [FIXUPP _s]        add ax, [s+4]    (s.inner.c)
```

Findings:
- Nested struct member access uses absolute byte offsets — no per-level indirection.
- `s.inner.b` is just `[_s + 2]` (same as if it were `s.b` in a flat struct).
- Nested-brace init `{1, {2, 3}}` writes consecutive 16-bit values.

