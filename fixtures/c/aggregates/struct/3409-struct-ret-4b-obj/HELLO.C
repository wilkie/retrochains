struct Two { int a; int b; };

struct Two make(int x, int y) {
  struct Two r;
  r.a = x;
  r.b = y;
  return r;
}
