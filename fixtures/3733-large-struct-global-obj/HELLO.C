struct P { int x; int y; };
struct P g = { 3, 4 };
int main(void) {
  return g.x + g.y;
}
