struct Point {
  int x;
  int y;
};
int main(void) {
  struct Point a[3];
  int i;
  a[0].x = 10;
  a[0].y = 11;
  a[1].x = 20;
  a[1].y = 21;
  a[2].x = 30;
  a[2].y = 31;
  i = 2;
  return a[i].x + a[i].y;
}
