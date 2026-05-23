struct W { int v; };
int mix(struct W w, int n);
int run(int x, int n) {
  struct W w;
  w.v = x;
  return mix(w, n);
}
