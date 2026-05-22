int helper(int x);
int compute(int a) {
  int b;
  int c;
  int d;
  b = a + 1;
  c = a + 2;
  d = helper(a);
  return b + c + d;
}
