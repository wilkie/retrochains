struct B { int x; };
struct A { struct B b; };
struct A s;
int main(void) {
  s.b.x = 42;
  return s.b.x;
}
