struct Inner { int v; };
struct Outer { struct Inner *p; };

int deep(struct Outer *o) {
  return o->p->v;
}
