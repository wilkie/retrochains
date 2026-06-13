struct P { int x; int y; };
int sum_pts(struct P *pts, int n) {
  int total = 0;
  int i;
  for (i = 0; i < n; i++) total += pts[i].x + pts[i].y;
  return total;
}
int main(void) {
  static struct P pts[3] = {{1,2},{3,4},{5,6}};
  return sum_pts(pts, 3);
}
