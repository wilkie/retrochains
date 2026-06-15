struct S { long x; long y; };
struct S s;
long g;
int main(void) {
  g = s.x + s.y;
  return 0;
}
