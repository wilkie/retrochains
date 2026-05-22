int main(void) {
  int a[5];
  int *p;
  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  p = &a[2];
  return p[-1];
}
