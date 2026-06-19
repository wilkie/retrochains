struct A { int x; int y; };
struct B { struct A a; int z; };
void add_x(struct B *b, int v) { b->a.x += v; }
void sub_y(struct B *b, int v) { b->a.y -= v; }
int main(void) {
  struct B b;
  b.a.x = 10;
  add_x(&b, 3);
  return b.a.x;
}
