int run(int n) {
  int s;
  s = 0;
  do {
    s = s + 1;
    n = n - 1;
  } while (n > 0);
  return s;
}
