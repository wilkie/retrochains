struct Inner { int a; int b; };
struct Outer { int id; struct Inner inner; int tail; };
int main(void) {
  static struct Outer o = {1, {10, 20}, 100};
  return o.id + o.inner.a + o.inner.b + o.tail;
}
