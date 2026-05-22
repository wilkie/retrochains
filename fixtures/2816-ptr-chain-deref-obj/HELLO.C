struct Inner { int v; };
struct Outer { struct Inner *p; };
int peek(struct Outer *o) {
  return o->p->v;
}
