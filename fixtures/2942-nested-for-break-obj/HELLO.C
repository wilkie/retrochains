int search(int n, int m) {
  int i;
  int j;
  for (i = 0; i < n; i = i + 1) {
    for (j = 0; j < m; j = j + 1) {
      if (i + j == 7) break;
    }
  }
  return i;
}
