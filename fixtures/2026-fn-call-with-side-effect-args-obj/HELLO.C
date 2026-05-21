int order = 0;
int trace(int v) { order = order * 10 + v; return v; }
int add(int a, int b) { return a + b; }
int main(void) {
  int r = add(trace(1), trace(2));
  return order * 100 + r;
}
