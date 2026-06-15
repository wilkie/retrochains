int op_add(int a, int b) { return a + b; }
int op_sub(int a, int b) { return a - b; }
int main(void) {
  int (*ops[2])(int, int);
  ops[0] = op_add;
  ops[1] = op_sub;
  return ops[0](10, 5) * 100 + ops[1](10, 5);
}
