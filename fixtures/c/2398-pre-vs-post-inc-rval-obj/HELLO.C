int main(void) {
  int x;
  int r1;
  int r2;
  x = 5;
  r1 = ++x;
  r2 = x++;
  return r1 * 100 + r2 * 10 + x;
}
