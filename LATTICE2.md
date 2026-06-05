Lattice 2.0 is a two-phase compiler where each phase is its own binary.

To compile a simple `HELLO.C` do the following:

```
// HELLO.C
int main(void) {
  return 0;
}
```

First pass to generate intermediate code:

```
LC1 HELLO.C
```

This generates `HELLO.Q` which is the input to the second phase:

```
LC2 HELLO.Q
```

This generates `HELLO.OBJ` which can now be linked.

```
LINK CS+HELLO,HELLO,NUL,LCS
```

Which specifies the objects to link (`CS` is the standard C start library and must be specified first and it implies `CS.OBJ` as the file it searchs for and uses), the name of the output executable (`HELLO` means `HELLO.EXE`), a `NUL` to skip generation of the link map, and then `LCS` as the libraries to find external symbols (`LCS` means an implicit search for `LCS.LIB`).

This will create, as noted, `HELLO.EXE` which is the resulting executable.
