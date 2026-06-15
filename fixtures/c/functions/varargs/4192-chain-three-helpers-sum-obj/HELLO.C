int add3(int a, int b, int c) {
  return a + b + c;
}
int mul2(int a, int b) {
  return a * b;
}
int combine(int x, int y, int z, int w) {
  return x + y + z + w;
}
int main(void) {
  int r;
  r = combine(add3(1, 2, 3), mul2(2, 3), add3(4, 0, 1), mul2(1, 1));
  return r;
}
