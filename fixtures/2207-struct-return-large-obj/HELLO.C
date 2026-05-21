struct Big { int a; int b; int c; int d; };
struct Big make_big(int x) {
  struct Big b;
  b.a = x;
  b.b = x + 1;
  b.c = x + 2;
  b.d = x + 3;
  return b;
}
int main(void) {
  struct Big bg = make_big(10);
  return bg.a + bg.b + bg.c + bg.d;
}
