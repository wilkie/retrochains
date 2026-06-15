typedef struct { int x; int y; } Point;
Point p;
int main(void) {
  p.x = 7;
  p.y = 9;
  return p.x + p.y;
}
