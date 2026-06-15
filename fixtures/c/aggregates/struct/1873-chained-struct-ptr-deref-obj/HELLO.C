struct Inner { int v; };
struct Outer { struct Inner *p; };
int main(void) {
  struct Inner i;
  struct Outer o;
  i.v = 42;
  o.p = &i;
  return o.p->v;
}
