struct Point {
  int x;
  int y;
};
struct Point pts[3] = {{1, 2}, {3, 4}, {5, 6}};
int main(void) {
  return pts[0].x + pts[1].y + pts[2].x;
}
