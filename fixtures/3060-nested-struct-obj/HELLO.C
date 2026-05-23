struct In { int v; };
struct Out { int tag; struct In inner; };
struct Out g;
int peek(void) {
  return g.inner.v;
}
