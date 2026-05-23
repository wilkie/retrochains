struct P { int x; };
int get_x(void *raw) {
  struct P *p = (struct P *)raw;
  return p->x;
}
