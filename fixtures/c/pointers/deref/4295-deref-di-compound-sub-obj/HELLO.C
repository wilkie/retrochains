int main() {
  int a;
  int b;
  int *p;
  int *q;
  a = 100;
  b = 200;
  p = &a;
  q = &b;
  *p -= 1000;
  *q -= 2000;
  return a + b;
}
