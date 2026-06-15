void modify(int *p);
int caller(void) {
  int x;
  x = 10;
  modify(&x);
  return x;
}
