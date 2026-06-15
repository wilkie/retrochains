int f(int a, int b, int c, int d) {
  int x = 5;
  ++x;
  return a + b + c + d + x + a + b + c + d;
}
int main(void) { return f(1, 2, 3, 4); }
