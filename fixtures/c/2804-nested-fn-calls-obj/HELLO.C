int inc1(int x);
int inc2(int x);
int both(int v) {
  return inc1(inc2(v));
}
