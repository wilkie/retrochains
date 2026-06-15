struct S { int x; };
int main(void) {
  struct S a;
  struct S *p;
  int v;
  p = &a;
  v = 42;
  p->x = v;
  return p->x;
}
