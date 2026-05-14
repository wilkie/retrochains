struct point { int x; int y; };
int main(void) {
  struct point pts[3];
  pts[1].x = 7;
  pts[1].y = 11;
  return pts[1].x + pts[1].y;
}
