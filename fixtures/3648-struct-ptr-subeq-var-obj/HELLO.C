struct Pt { int x; int y; };

struct Pt *back(struct Pt *p, int n) {
  return p - n;
}
