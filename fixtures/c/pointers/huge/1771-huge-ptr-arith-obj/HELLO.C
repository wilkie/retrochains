int main(void) {
  int a[3];
  int huge *p;
  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  p = (int huge *)a;
  p++;
  return *p;
}
