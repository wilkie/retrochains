struct Inner { int x; };
struct Outer { struct Inner i; };

void put(struct Outer *o, int v) {
  o->i.x = v;
}
