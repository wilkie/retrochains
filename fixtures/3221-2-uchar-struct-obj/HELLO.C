struct P { unsigned char a; unsigned char b; };
struct P g;
int sum(void) {
  return g.a + g.b;
}
