struct S { int x; int y; };
void use(struct S *p);

void driver(void) {
  struct S s;
  s.x = 5;
  s.y = 10;
  use(&s);
}
