int v = 42;
struct W { int *p; };
struct W g = { &v };
int peek(void) {
  return *(g.p);
}
