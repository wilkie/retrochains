int (*ops[3])(int);
int dispatch(int i, int x) {
  return ops[i](x);
}
