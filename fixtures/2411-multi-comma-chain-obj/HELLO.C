int main(void) {
  int a;
  int b;
  int c;
  int r;
  a = 0;
  b = 0;
  c = 0;
  r = (a = 1, b = 2, c = 3, a + b + c);
  return r;
}
