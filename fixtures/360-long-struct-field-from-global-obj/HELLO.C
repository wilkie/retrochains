struct S { long x; };
struct S s;
long g;
int main(void) {
  s.x = g;
  return 0;
}
