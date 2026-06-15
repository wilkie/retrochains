struct S { long x; };
struct S s;
long g;
long f(void) {
  return g + s.x;
}
int main(void) {
  return 0;
}
