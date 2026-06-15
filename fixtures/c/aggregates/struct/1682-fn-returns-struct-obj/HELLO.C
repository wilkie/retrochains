struct P { int x; int y; };
struct P mk(void) {
  struct P p = {3, 4};
  return p;
}
int main(void) {
  struct P r = mk();
  return r.x + r.y;
}
