int main(void) {
  int a;
  int b;
  int *p[2];
  a = 5;
  b = 10;
  p[0] = &a;
  p[1] = &b;
  *p[1] = 99;
  return b;
}
