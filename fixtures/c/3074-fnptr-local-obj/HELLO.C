int double_it(int x) { return x + x; }
int run(int n) {
  int (*op)(int);
  op = double_it;
  return op(n);
}
