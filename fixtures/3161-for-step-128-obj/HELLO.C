int sum(int n) {
  int i;
  int s;
  s = 0;
  for (i = 0; i < n; i += 128) {
    s = s + i;
  }
  return s;
}
