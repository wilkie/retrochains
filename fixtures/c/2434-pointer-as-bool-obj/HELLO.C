int main(void) {
  int x;
  int *p;
  int r;
  x = 7;
  p = &x;
  if (p) {
    r = 100;
  } else {
    r = 200;
  }
  return r;
}
