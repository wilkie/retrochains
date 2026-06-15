struct S { int x; int y; };
int main(void) {
  int a = 10;
  int b = 20;
  struct S s;
  s.x = a + b;
  return s.x;
}
