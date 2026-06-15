int main(void) {
  int x, y, a[3];
  x = 4;
  y = 6;
  a[0] = x;
  a[1] = y;
  a[2] = x + y;
  return a[2] + a[0] + a[1];
}
