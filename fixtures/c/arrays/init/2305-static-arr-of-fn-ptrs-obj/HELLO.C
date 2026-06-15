int a(void) { return 1; }
int b(void) { return 2; }
int c(void) { return 3; }
int main(void) {
  static int (*fns[3])(void) = {a, b, c};
  return fns[0]() + fns[1]() + fns[2]();
}
