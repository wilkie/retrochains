struct P { int x; int y; };
int *get_y_ptr(void) {
  struct P p;
  p.y = 99;
  return &p.y;
}
