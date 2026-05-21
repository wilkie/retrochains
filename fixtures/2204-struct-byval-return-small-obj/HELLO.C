struct Point { int x; int y; };
struct Point make_pt(int a, int b) {
  struct Point p;
  p.x = a;
  p.y = b;
  return p;
}
int main(void) {
  struct Point p = make_pt(3, 4);
  return p.x + p.y;
}
