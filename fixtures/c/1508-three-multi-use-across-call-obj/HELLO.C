int dbl(int x) { return x + x; }
int main(void) {
  int a = 1;
  int b = 2;
  int c = 3;
  a = a + 1;
  b = b + 1;
  c = c + 1;
  c = dbl(c);
  return a + b + c;
}
