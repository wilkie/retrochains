struct Pt { int x; int y; };

void bump(struct Pt **pp) {
  *pp += 1;
}
