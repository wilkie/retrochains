struct S { long x; };
struct S s;
long f(void) {
  return s.x;
}
int main(void) {
  return 0;
}
