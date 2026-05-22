int (*op)(int, int);
int call(int a, int b) {
  return op(a, b);
}
