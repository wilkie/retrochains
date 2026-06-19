int main() {
  int a;
  int b;
  int *p;
  int *q;
  a = 100;
  b = 200;
  p = &a;
  q = &b;
  *p ^= 5;
  *q ^= 9;
  return a + b;
}
