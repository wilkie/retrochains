struct R { int x; int y; };
struct R g = { 11, 22 };
int main(void) {
  return g.x + g.y;
}
