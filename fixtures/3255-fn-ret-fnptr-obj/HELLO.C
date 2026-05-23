int target(int x) { return x; }
int (*get_op(void))(int) {
  return target;
}
