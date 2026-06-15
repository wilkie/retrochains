struct P { int x; int y; };
int test(struct P *p) {
  p->x = 100;
  p->y = 200;
  return p->x + p->y;
}
