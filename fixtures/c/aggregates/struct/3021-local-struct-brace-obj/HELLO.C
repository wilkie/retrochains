struct P { int x; int y; };
int sum(void) {
  struct P p;
  p.x = 10;
  p.y = 20;
  return p.x + p.y;
}
