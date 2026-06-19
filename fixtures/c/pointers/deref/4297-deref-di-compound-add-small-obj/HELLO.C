int main() {
  int a;
  int b;
  int *p;
  int *q;
  a = 100;
  b = 200;
  p = &a;
  q = &b;
  *p += 9;
  *q += 3;
  return a + b;
}
