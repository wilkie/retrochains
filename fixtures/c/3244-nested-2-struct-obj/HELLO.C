struct A { int x; };
struct B { struct A a; int y; };
struct C { struct B b; int z; };
struct C c;
int peek(void) {
  return c.b.a.x;
}
