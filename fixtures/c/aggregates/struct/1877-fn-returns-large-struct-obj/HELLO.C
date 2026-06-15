struct T { int a; int b; int c; };
struct T make_t(void) {
  struct T r;
  r.a = 10;
  r.b = 20;
  r.c = 30;
  return r;
}
int main(void) {
  struct T t;
  t = make_t();
  return t.a + t.b + t.c;
}
