int main() {
  int a;
  int b;
  int *p;
  int *q;
  a = 100;
  b = 200;
  p = &a;
  q = &b;
  *p &= 12;
  *q &= 6;
  return a + b;
}
