struct S { int x; int y; };
long g;
struct S s;
int main() {
  g = 100;
  s.x = 50;
  g += s.x;
  return 0;
}
