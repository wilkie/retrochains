struct S { long x; };
struct S s;
long g;
int main(void) {
  g = s.x;
  return 0;
}
