struct S { int x; int y; };
struct S s;
int main(void) {
  int v;
  v = 42;
  s.x = v;
  return s.x;
}
