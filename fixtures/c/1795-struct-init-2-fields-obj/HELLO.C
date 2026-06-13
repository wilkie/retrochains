struct P { int x; int y; };
int main(void) {
  struct P p = {10, 20};
  return p.x + p.y;
}
