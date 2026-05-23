struct P { int x; int y; };
struct P make(int a, int b) {
  struct P p;
  p.x = a;
  p.y = b;
  return p;
}
