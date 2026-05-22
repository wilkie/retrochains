struct Q { int a; int b; char c; };
struct Q *next(struct Q *p, int n) {
  return p + n;
}
