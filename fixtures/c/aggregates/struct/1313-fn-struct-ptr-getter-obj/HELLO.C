struct S { int x; };
struct S s;
int getX(struct S *p) {
  return p->x;
}
int main(void) {
  s.x = 42;
  return getX(&s);
}
