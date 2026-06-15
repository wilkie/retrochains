struct R { int a; int b; int c; };
struct R mk(void) {
  struct R r;
  r.a = 1;
  r.b = 2;
  r.c = 3;
  return r;
}
int main(void) {
  struct R x = mk();
  return x.a + x.b + x.c;
}
