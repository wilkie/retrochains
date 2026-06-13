int sum(int n) {
  register int i;
  int s = 0;
  for (i = 0; i < n; i++) s += i;
  return s;
}
