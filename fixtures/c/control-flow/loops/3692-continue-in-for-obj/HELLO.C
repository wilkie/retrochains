int sum_evens(int n) {
  int i, s;
  s = 0;
  for (i = 0; i < n; i++) {
    if (i & 1) continue;
    s += i;
  }
  return s;
}
