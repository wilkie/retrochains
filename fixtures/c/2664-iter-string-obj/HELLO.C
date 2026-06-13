int strlen_simple(const char *s) {
  int n;
  n = 0;
  while (*s) {
    n = n + 1;
    s = s + 1;
  }
  return n;
}
