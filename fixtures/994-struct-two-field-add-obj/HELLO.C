struct S { int a; int b; };
int main(void) {
  struct S s;
  s.a = 5;
  s.b = 10;
  return s.a + s.b;
}
