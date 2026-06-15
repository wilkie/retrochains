int f(int x);
int g(int x);
int both(int v) {
  f(v);
  return g(v);
}
