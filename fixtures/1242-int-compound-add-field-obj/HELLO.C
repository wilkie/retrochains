struct S { int x; };
struct S s;
int main(void) {
  int a;
  s.x = 3;
  a = 10;
  a += s.x;
  return a;
}
