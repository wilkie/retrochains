struct W { int v; };
int extract(struct W w);
int caller(int x) {
  struct W w;
  w.v = x;
  return extract(w);
}
