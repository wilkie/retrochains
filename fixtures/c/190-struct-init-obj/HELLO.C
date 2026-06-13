struct point { int x; int y; };
struct point g = {3, 7};
int main(void) {
  return g.x + g.y;
}
