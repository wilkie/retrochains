int sum(int *p, int n) {
  int s = 0;
  while (n--) s += *p++;
  return s;
}
