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

