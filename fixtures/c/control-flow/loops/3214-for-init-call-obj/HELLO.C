int get_n(void);
int test(void) {
  int i;
  int s;
  s = 0;
  for (i = get_n(); i > 0; i = i - 1) {
    s = s + i;
  }
  return s;
}
