struct P { int x; int y; };
struct P g;
int main(void) {
  g.x = 5;
  g.y = 7;
  return g.x + g.y;
}
