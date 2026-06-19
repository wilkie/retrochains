struct A { int x; int y; };
struct B { struct A a; int z; };
int rd_ax(struct B *b) { return b->a.x; }
int rd_ay(struct B *b) { return b->a.y; }
int main(void) {
  struct B b;
  b.a.x = 5;
  return rd_ax(&b);
}
