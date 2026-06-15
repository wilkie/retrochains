struct P { int x; int y; };
int main(void) {
  struct P arr[2];
  arr[0].x = 1;
  arr[1].y = 4;
  return arr[0].x + arr[1].y;
}
