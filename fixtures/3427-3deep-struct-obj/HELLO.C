struct Inner { int v; };
struct Middle { struct Inner inner; };
struct Outer { struct Middle middle; } o;

int deep(void) {
  return o.middle.inner.v;
}
