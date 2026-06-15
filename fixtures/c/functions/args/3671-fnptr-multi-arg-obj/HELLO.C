int apply(int (*op)(int, int), int a, int b) {
  return op(a, b);
}
