int helper(int x);
int call(int v) {
  return helper(v > 0 ? v : -v);
}
