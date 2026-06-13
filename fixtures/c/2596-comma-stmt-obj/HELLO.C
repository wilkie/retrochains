int main(void) {
  int x;
  int y;
  x = 1;
  y = 2;
  x = (x = 10, y = 20, x + y);
  return x;
}
