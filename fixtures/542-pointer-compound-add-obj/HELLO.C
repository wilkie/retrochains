int main(void) {
  int a[5];
  int *p;
  p = a;
  p += 2;
  *p = 42;
  return 0;
}
