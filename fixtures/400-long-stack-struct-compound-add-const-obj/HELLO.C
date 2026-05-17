struct S { long x; };
int main(void) {
  struct S s;
  s.x = 0;
  s.x += 5;
  return 0;
}
