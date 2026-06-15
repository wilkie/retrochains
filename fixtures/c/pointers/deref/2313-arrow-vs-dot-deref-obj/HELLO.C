struct P { int x; int y; };
int main(void) {
  static struct P p = {10, 20};
  struct P *pp = &p;
  return p.x + pp->y;
}
