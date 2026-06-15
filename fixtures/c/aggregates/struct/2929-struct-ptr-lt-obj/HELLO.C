struct P { int v; };
int before(struct P *a, struct P *b) {
  if (a < b) return 1;
  return 0;
}
