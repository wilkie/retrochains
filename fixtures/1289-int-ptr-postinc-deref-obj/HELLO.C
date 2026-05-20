int main(void) {
  int a[3];
  int *p;
  int v;
  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  p = a;
  v = *p++;
  return v;
}
