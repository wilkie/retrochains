struct Three { int a; int b; int c; };

struct Three make(int x, int y, int z) {
  struct Three r;
  r.a = x;
  r.b = y;
  r.c = z;
  return r;
}
