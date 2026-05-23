struct P { int x; int y; };
int sum(struct P p);
int caller(int a, int b) {
  struct P p;
  p.x = a;
  p.y = b;
  return sum(p);
}
