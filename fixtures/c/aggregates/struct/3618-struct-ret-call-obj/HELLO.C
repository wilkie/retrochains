struct Two { int a; int b; };
struct Two make(void);

int driver(void) {
  struct Two t = make();
  return t.a + t.b;
}
