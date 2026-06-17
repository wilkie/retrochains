struct S { int a; int b; };
void f(struct S *p, int v) {
  p->b += v;
}
