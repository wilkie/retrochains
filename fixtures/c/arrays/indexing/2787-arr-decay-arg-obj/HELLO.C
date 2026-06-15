int sumof(int *p, int n);
int call_with_arr(void) {
  int a[3];
  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  return sumof(a, 3);
}
