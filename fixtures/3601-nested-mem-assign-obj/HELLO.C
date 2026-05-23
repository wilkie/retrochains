struct Inner { int v; };
struct Outer { struct Inner inner; } o;

void put(int v) {
  o.inner.v = v;
}
