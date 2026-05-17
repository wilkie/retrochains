struct S { int x; };
struct S s;
int main(void) {
  s.x &= 0xFF;
  return 0;
}
