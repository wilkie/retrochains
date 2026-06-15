int main(void) {
  volatile int v;
  int a;
  int b;
  v = 7;
  a = v;
  b = v;
  return a + b;
}
