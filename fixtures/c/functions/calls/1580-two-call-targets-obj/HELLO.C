int dbl(int x) { return x + x; }
int trp(int x) { return x + x + x; }
int main(void) {
  int a = 5;
  int b = 7;
  a = a + 1;
  b = b + 1;
  a = dbl(a);
  b = trp(b);
  return a + b;
}
