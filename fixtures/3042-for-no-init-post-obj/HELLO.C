int run(int n) {
  int s;
  s = 0;
  for (; n > 0; ) {
    s = s + n;
    n = n - 1;
  }
  return s;
}
