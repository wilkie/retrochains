int countdown(int n) {
  int i;
  int s;
  s = 0;
  for (i = n; i > 0; i = i - 1) {
    s = s + i;
  }
  return s;
}
