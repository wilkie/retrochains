struct Inner { int c; };
struct Mid { struct Inner *p; };
struct Outer { struct Mid m; };

int grab(struct Outer *o) {
  return o->m.p->c;
}
