struct P { int x; int y; };
int main(void) {
  struct P a[2];
  a[0].x = 1;
  a[0].y = 2;
  a[1].x = 10;
  a[1].y = 20;
  return a[1].x + a[1].y - a[0].x;
}
