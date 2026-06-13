int invoke(int (*op)(int), int v) {
  return op(v);
}
