int sum2(int a, int b) { return a + b; }
int main(void) {
  int x = 5;
  int r = sum2((x = 10, x), (x = 20, x));
  return r + x;
}
