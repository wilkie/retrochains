int compute(int a, int b, int flag) {
  int x;
  x = flag ? (a = a + 1) : b;
  return x;
}
