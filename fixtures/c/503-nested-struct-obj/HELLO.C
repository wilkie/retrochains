struct inner { int a; int b; };
struct outer { int x; struct inner i; };
struct outer g;
int main(void) {
  g.x = 1;
  g.i.a = 2;
  g.i.b = 3;
  return 0;
}
