int counter(void) {
  static int n = 0;
  n = n + 1;
  return n;
}
