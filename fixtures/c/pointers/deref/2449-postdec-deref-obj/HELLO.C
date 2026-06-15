int main(void) {
  int x;
  int *p;
  int r;
  x = 10;
  p = &x;
  r = (*p)--;
  return r * 100 + x;
}
