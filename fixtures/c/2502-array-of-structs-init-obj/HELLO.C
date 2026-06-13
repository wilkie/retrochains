struct Point { int x; int y; };
struct Point pts[2] = { { 1, 2 }, { 3, 4 } };
int main(void) {
  return pts[1].y;
}
