struct P { int x; int y; };
struct P arr[2] = {{1, 2}, {3, 4}};
int main(void) {
  return arr[1].x + arr[0].y;
}
