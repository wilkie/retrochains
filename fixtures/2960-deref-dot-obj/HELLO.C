struct P { int x; };
int extract(struct P *p) {
  return (*p).x;
}
