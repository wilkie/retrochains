int count(int n) {
  int i;
  int s;
  s = 0;
  for (i = 0; i < n; i = i + 1) {
    if (i == 3) continue;
    s = s + i;
  }
  return s;
}
