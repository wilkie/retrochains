int f(long x) { return 0; }
struct S { long x; };
struct S s;
int main(void) {
  f(s.x);
  return 0;
}
