struct W { int v; };
struct W make(int x) {
  struct W w;
  w.v = x * 2;
  return w;
}
