int main(void) {
  int a[2];
  int *p;
  int x;
  a[0] = 3;
  a[1] = 4;
  p = a;
  x = 5;
  return x + *p;
}
