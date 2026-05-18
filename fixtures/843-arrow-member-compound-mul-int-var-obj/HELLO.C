struct S { int x; int y; };
struct S g;
int main() {
  int y;
  struct S *p;
  g.x = 10;
  y = 5;
  p = &g;
  p->x *= y;
  return 0;
}
