int main(void) {
  static int a[10];
  int *p = &a[5];
  int *q = p + 2;
  int *r = p - 2;
  return (int)(q - r);
}
