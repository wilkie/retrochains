int main(void) {
  static int a[5] = {10, 20, 30, 40, 50};
  int *p = a;
  int *q = (int *)((char *)p + 4);
  return *q;
}
