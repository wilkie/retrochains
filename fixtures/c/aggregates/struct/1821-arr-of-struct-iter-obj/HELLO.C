struct P { int x; int y; };
int main(void) {
  struct P a[3];
  int sum = 0;
  int i;
  a[0].x = 1; a[0].y = 2;
  a[1].x = 3; a[1].y = 4;
  a[2].x = 5; a[2].y = 6;
  for (i = 0; i < 3; i++) sum += a[i].x + a[i].y;
  return sum;
}
