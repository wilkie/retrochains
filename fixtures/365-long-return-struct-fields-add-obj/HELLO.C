struct S { long x; long y; };
struct S s;
long f(void) {
  return s.x + s.y;
}
int main(void) {
  return 0;
}
