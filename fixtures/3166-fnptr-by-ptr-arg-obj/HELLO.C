int double_it(int x) { return x + x; }
int call_with(int (*fn)(int), int v) {
  return fn(v);
}
int run(void) {
  return call_with(double_it, 5);
}
