struct W { int v; };
int copy(void) {
  struct W a;
  struct W b;
  a.v = 42;
  b = a;
  return b.v;
}
