struct P { int x; int y; };
const struct P g = { 10, 20 };
int peek(void) {
  return g.x;
}
