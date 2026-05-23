int sum_n(int n) {
  int s;
  s = 0;
  do {
    s = s + n;
    n = n - 1;
  } while (n > 0);
  return s;
}
