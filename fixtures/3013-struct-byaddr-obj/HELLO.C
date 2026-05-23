struct M { int a; int b; int c; };
int sum(struct M *m);
int caller(void) {
  struct M v;
  v.a = 1;
  v.b = 2;
  v.c = 3;
  return sum(&v);
}
