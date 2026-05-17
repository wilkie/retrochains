struct S { long x; };
struct S s;
long y;
int main(void) {
  s.x /= y;
  return 0;
}
