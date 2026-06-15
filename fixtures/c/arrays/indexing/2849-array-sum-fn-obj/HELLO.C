int total(int *a, int n) {
  int i;
  int s;
  s = 0;
  for (i = 0; i < n; i = i + 1) {
    s = s + a[i];
  }
  return s;
}
