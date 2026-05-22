int main(void) {
  int x;
  int y;
  int a[3];
  x = 1;
  y = 2;
  a[0] = x;
  a[1] = y;
  a[2] = x + y;
  return a[0] + a[1] + a[2];
}
