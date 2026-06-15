typedef int (*BinOp)(int, int);
int add(int a, int b) { return a + b; }
int mul(int a, int b) { return a * b; }
int apply(BinOp f, int a, int b) { return f(a, b); }
int main(void) {
  return apply(add, 3, 4) + apply(mul, 5, 6);
}
