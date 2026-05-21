struct P { int x; int y; };
int main(void) {
  struct P arr[3] = {{1,2}, {3,4}, {5,6}};
  return arr[0].x + arr[1].y + arr[2].x;
}
