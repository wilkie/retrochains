int matrix_sum(int n, int m) {
  int i;
  int j;
  int s;
  s = 0;
  for (i = 0; i < n; i = i + 1) {
    for (j = 0; j < m; j = j + 1) {
      s = s + 1;
    }
  }
  return s;
}
