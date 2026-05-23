struct P { int v; };
int same(struct P *a, struct P *b) {
  if (a == b) return 1;
  return 0;
}
