struct P { int x; int y; };
struct P pts[3];
int g;
int main(void) {
  pts[1].x = 7;
  g = pts[1].x;
  return 0;
}
