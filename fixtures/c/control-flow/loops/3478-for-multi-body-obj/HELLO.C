int compute(int n) {
  int s = 0;
  int p = 1;
  int i;
  for (i = 1; i <= n; i++) {
    s += i;
    p *= i;
  }
  return s + p;
}
