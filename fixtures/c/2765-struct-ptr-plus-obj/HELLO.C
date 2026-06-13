struct P { int x; int y; };
struct P *next(struct P *p, int n) {
  return p + n;
}
