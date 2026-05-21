int helper(int x) { return x + 1; }
int main(void) {
  int a = 10;
  int b = 20;
  int c = 30;
  int r = helper(b);
  return a + c + r;
}
