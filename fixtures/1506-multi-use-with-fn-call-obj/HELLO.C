int inc(int x) { return x + 1; }
int main(void) {
  int a = 10;
  int b = 20;
  a = a + 1;
  b = inc(b);
  return a + b;
}
