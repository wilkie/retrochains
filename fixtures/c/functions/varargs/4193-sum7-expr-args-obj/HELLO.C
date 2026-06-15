int sum7(int a, int b, int c, int d, int e, int f, int g) {
  return a + b + c + d + e + f + g;
}
int main(void) {
  int x;
  int y;
  x = 3;
  y = 4;
  return sum7(x, y, x + y, x * 2, y - 1, x + y + 1, 2);
}
