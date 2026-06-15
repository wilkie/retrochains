struct C { int v; };
struct B { struct C c; };
struct A { struct B b; };
struct A obj;
int main(void) {
  obj.b.c.v = 42;
  return obj.b.c.v;
}
