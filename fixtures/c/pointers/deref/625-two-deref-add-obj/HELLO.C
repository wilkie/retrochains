int main(void) {
  int a;
  int b;
  int *p;
  int *q;
  a = 5;
  b = 7;
  p = &a;
  q = &b;
  return *p + *q;
}
