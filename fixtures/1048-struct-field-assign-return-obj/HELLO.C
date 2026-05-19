struct S { int x; int y; };
int main(void) {
  struct S s;
  s.x = 42;
  s.y = 17;
  return s.x;
}
