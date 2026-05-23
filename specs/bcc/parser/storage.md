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

