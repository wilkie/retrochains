struct S { int x; int y; };
struct S g = { 13, 14 };
int main(void) {
  return g.x + g.y;
}
