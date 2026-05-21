int main(void) {
  int a[3];
  int huge *p;
  a[0] = 100;
  a[1] = 200;
  a[2] = 300;
  p = (int huge *)&a[2];
  p--;
  return *p;
}
