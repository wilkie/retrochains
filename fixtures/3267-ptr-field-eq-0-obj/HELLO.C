struct P { int x; int y; };
int test(struct P *p) {
  if (p->x == 0) return 1;
  return 0;
}
