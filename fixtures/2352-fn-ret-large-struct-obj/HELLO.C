struct Big {
  int a;
  int b;
  int c;
  int d;
};
struct Big make(void) {
  struct Big r;
  r.a = 1;
  r.b = 2;
  r.c = 3;
  r.d = 4;
  return r;
}
int main(void) {
  struct Big x;
  x = make();
  return x.a + x.b + x.c + x.d;
}
