struct P { int x; int y; };
struct P make_p(void) {
  struct P r;
  r.x = 10;
  r.y = 20;
  return r;
}
int main(void) {
  struct P p;
  p = make_p();
  return p.x + p.y;
}
