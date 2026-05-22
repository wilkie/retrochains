int counter(void) {
  static int n = 42;
  n = n + 1;
  return n;
}
int main(void) {
  int a;
  int b;
  a = counter();
  b = counter();
  return a + b;
}
