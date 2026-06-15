struct P { int x; int y; };
int main(void) {
  static struct P pts[3] = {{1, 2}, {3, 4}, {5, 6}};
  return pts[0].x + pts[1].y + pts[2].x;
}
