int main(void) {
  int a;
  int b;
  int c;
  int r;
  a = 5;
  b = 10;
  c = 20;
  r = (a++ > 0) ? b : c;
  return r + a;
}
