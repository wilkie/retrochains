int main(void) {
  int a[2];
  int *p;
  a[0] = 3;
  a[1] = 4;
  p = a;
  return 10 - *p;
}
