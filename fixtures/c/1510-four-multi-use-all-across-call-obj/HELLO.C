int dbl(int x) { return x + x; }
int main(void) {
  int a = 1;
  int b = 2;
  int c = 3;
  int d = 4;
  a = a + 1;
  b = b + 1;
  c = c + 1;
  d = d + 1;
  d = dbl(d);
  return a + b + c + d;
}
