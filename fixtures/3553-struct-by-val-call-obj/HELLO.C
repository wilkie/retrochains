struct Pt { int x; int y; };
int sum(struct Pt p);

int driver(void) {
  struct Pt q;
  q.x = 3;
  q.y = 4;
  return sum(q);
}
