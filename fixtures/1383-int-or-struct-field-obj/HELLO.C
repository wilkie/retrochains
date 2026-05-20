struct S { int x; };
struct S s = {0x0f};
int main(void) {
  int a = 0xf0;
  a |= s.x;
  return a;
}
