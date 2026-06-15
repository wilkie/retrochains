int order = 0;
int log(int v) { order = order * 10 + v; return v; }
int sum3(int a, int b, int c) { return a + b + c; }
int main(void) {
  int r = sum3(log(1), log(2), log(3));
  return order * 100 + r;
}
