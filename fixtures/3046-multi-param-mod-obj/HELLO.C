int swap_add(int a, int b) {
  int t;
  t = a;
  a = b;
  b = t;
  return a + b;
}
