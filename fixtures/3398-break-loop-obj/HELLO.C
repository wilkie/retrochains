int find(int *p, int n, int target) {
  int i;
  for (i = 0; i < n; i++) {
    if (p[i] == target) break;
  }
  return i;
}
