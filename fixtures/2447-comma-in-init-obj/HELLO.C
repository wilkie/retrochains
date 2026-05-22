int main(void) {
  int x;
  int r;
  x = 0;
  r = (x = 5, x + 10);
  return r;
}
