struct Pt { int x; int y; };

struct Pt *advance(struct Pt *p) {
  return p++;
}
