int compute(int (*op)(int, int, int), int a, int b, int c) {
  return op(a, b, c);
}
