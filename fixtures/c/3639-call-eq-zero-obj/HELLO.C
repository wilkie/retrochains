int f(void);

int chk(void) {
  if (f() == 0) return 1;
  return 0;
}
