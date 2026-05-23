int sum(int n) {
  int s = 0;
  int i;
  for (i = 0; i < n; ) {
    s += i;
    i++;
  }
  return s;
}
