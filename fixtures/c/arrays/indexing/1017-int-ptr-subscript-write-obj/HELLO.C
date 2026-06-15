int main(void) {
  int a[3];
  int *p;
  p = a;
  p[1] = 99;
  return a[1];
}
