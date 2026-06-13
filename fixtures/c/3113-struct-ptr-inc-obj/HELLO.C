struct S { int a; int b; int c; };
struct S *next(struct S *p) {
  p++;
  return p;
}
