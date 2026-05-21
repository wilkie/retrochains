struct P { int x; int y; };
int main(void) {
  struct P a, b;
  a.x = 10;
  a.y = 20;
  b = a;
  return b.x + b.y;
}
