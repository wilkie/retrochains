struct S { int x; };
struct S s;
int main(void) {
  s.x = 5;
  if (s.x == 5) return 7;
  return 0;
}
