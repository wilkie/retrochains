int a, b;
int count(void) {
  int n = 0;
  while (a > 0 || b > 0) { n = n + 1; a = a - 1; b = b - 1; }
  return n;
}
