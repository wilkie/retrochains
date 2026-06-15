struct S { int a; int b; char c; };
struct S *next(struct S *p) {
  p++;
  return p;
}
