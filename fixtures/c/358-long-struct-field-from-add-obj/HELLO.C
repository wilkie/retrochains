struct S { long x; };
struct S s;
long g;
long h;
int main(void) {
  s.x = g + h;
  return 0;
}
