int test(int n, int m) {
  int i;
  int j;
  int s;
  s = 0;
  for (i = 0; i < n; i = i + 1) {
    for (j = 0; j < m; j = j + 1) {
      if (j == 5) break;
      s = s + 1;
    }
  }
  return s;
}
