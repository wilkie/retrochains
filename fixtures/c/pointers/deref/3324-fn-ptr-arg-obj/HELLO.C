int add1(int x) { return x + 1; }

int (*get_add1(void))(int) {
  return add1;
}
