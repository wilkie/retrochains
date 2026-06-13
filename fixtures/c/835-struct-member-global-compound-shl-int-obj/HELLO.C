struct S { int x; int y; };
struct S s;
int main() {
  int y;
  s.x = 10;
  y = 2;
  s.x <<= y;
  return 0;
}
