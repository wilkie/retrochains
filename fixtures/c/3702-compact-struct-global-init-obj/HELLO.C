struct P { int x; int y; };
struct P p = { 3, 4 };
int main(void) {
  return p.x + p.y;
}
