int test(int n) {
  int i;
  int s;
  i = 0;
  s = 0;
  while (i < n) {
    i = i + 1;
    if (i == 3) continue;
    s = s + i;
  }
  return s;
}
