struct P { int x; int y; };
struct P arr[3];
int main(void) {
  arr[1].x = 7;
  arr[1].y = 9;
  return arr[1].x + arr[1].y;
}
