int sum_n(int n) {
  int a[5];
  int i;
  int s;
  for (i = 0; i < n; i = i + 1) {
    a[i] = i;
  }
  s = 0;
  for (i = 0; i < n; i = i + 1) {
    s = s + a[i];
  }
  return s;
}
