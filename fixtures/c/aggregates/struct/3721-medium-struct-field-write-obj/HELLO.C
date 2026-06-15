struct P { int x; int y; };
int main(void) {
  struct P p;
  p.x = 7;
  p.y = 11;
  return p.x + p.y;
}
