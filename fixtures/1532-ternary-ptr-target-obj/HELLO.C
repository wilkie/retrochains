int main(void) {
  int x = 5;
  int y = 10;
  int *p = (x < y) ? &x : &y;
  *p = 99;
  return x;
}
