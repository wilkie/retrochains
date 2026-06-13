int main(void) {
  int a[3];
  int far *p;
  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  p = (int far *)a;
  p++;
  return *p;
}
