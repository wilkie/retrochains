int sum(int n) {
  int i;
  int s;
  s = 0;
  for (i = n * 2; i > 0; i = i - 1) {
    s = s + i;
  }
  return s;
}
