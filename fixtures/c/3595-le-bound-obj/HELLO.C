int sum_inclusive(int n) {
  int s = 0, i;
  for (i = 0; i <= n; i++) s += i;
  return s;
}
