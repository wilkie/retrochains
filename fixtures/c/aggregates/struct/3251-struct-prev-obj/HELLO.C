struct P { int x; int y; };
int prev_x(struct P *p) {
  return (p - 1)->x;
}
