struct Point { int x; int y; };
int sum_pt(struct Point p) {
  return p.x + p.y;
}
int main(void) {
  struct Point pt;
  pt.x = 10;
  pt.y = 20;
  return sum_pt(pt);
}
