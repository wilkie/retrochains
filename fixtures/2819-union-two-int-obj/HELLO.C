union U { int a; int b; };
union U u;
int test(void) {
  u.a = 100;
  return u.b;
}
