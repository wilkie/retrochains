struct One { int a; };

struct One make(int v) {
  struct One r;
  r.a = v;
  return r;
}
