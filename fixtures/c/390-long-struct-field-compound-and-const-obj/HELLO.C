struct S { long x; };
struct S s;
int main(void) {
  s.x &= 255;
  return 0;
}
