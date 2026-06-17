struct Inner { int x; int y; };
struct Outer { struct Inner i; };
void f(struct Outer *o, int v) {
  o->i.y = v;
}
