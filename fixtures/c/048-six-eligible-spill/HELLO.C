int f(int a, int b, int c, int d) {
  int x = 5;
  int y = 0;
  while (a < 100) {
    a = a + 1;
    b = b + 1;
    c = c + 1;
    d = d + 1;
    x = x + 1;
    y = y + 1;
  }
  return a + b + c + d + x + y;
}
int main(void) { return f(1, 2, 3, 4); }
