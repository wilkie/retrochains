struct S { int a; int b; int c; };
int main(void) {
  static struct S s = {10, 20};
  return s.a + s.b + s.c;
}
