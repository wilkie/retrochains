int main(void) {
  int x;
  int r;
  x = 42;
  r = 0;
  switch (x) {
    default:
      r = 99;
      break;
  }
  return r;
}
