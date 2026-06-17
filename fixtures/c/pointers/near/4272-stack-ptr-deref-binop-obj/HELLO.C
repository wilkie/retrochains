int f(int *p, int *q) {
  return *p + *q;
}
int main(void) {
  int a;
  int b;
  a = 3;
  b = 4;
  return f(&a, &b);
}
