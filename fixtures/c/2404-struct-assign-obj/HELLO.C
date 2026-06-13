struct Point {
  int x;
  int y;
};
int main(void) {
  struct Point a;
  struct Point b;
  a.x = 10;
  a.y = 20;
  b = a;
  return b.x + b.y;
}
