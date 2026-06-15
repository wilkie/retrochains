int helper(int x) { return x; }
int main(void) {
  int a = 1, b = 2, c = 3;
  int r = a + b + c;
  r += helper(a);
  r += helper(b);
  r += helper(c);
  return r;
}
