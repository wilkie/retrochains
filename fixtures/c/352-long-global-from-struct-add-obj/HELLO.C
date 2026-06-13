struct S { long x; };
struct S s;
long g;
int main(void) {
  g = s.x + 5;
  return 0;
}
