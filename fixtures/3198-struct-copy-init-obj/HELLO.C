struct P { int x; int y; };
int test(struct P p) {
  struct P q = p;
  return q.x + q.y;
}
