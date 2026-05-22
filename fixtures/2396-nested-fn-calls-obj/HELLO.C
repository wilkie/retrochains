int dbl(int x) { return x + x; }
int add(int a, int b) { return a + b; }
int main(void) {
  return add(dbl(3), dbl(5));
}
