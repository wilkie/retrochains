struct S { int x; int y; };
int g;
struct S s;
int main() {
  g = 100;
  s.x = 2;
  g <<= s.x;
  return 0;
}
