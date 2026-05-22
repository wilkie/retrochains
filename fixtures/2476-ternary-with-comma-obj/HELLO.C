int main(void) {
  int c;
  int a;
  int b;
  int r;
  c = 1;
  a = 0;
  b = 0;
  r = c ? (a = 5, 10) : (b = 7, 20);
  return r + a + b;
}
