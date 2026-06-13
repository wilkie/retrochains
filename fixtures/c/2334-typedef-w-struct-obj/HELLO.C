typedef struct { int x; int y; } Point;
int main(void) {
  Point p;
  p.x = 7;
  p.y = 8;
  return p.x + p.y;
}
