struct Inner { int v; };
struct Outer { struct Inner inner; int extra; };
struct Outer g = { { 7 }, 11 };
int main(void) {
  return g.inner.v + g.extra;
}
