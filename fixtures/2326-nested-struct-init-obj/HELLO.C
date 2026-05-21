struct Inner { int a; int b; };
struct Outer { struct Inner i; int c; };
int main(void) {
  static struct Outer o = {{10, 20}, 30};
  return o.i.a + o.i.b + o.c;
}
