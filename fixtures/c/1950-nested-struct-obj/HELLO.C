struct Inner { int x; int y; };
struct Outer { int n; struct Inner inner; };
int main(void) {
  struct Outer o;
  o.n = 1;
  o.inner.x = 10;
  o.inner.y = 20;
  return o.n + o.inner.x + o.inner.y;
}
