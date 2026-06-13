struct T { int a; int b; int c; int d; };
int main(void) {
  struct T x, y;
  x.a = 1; x.b = 2; x.c = 3; x.d = 4;
  y = x;
  return y.a + y.b + y.c + y.d;
}
