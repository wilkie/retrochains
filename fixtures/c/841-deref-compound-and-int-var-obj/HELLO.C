int main() {
  int x;
  int y;
  int *p;
  x = 100;
  y = 15;
  p = &x;
  *p &= y;
  return 0;
}
