struct S { long l; };
struct S g;
int main() {
  int y;
  struct S *p;
  g.l = 100;
  y = 5;
  p = &g;
  p->l += y;
  return 0;
}
