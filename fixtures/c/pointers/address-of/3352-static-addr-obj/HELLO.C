int *holder(void) {
  static int x = 42;
  return &x;
}
