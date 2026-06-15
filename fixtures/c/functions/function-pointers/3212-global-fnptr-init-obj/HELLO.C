int target(int x) { return x + 1; }
int (*ptr)(int) = target;
int call_it(int v) {
  return ptr(v);
}
