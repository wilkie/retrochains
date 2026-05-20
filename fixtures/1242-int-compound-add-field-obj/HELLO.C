struct S { int x; };
struct S s;
int main(void) {
  s.x = 3;
  int a = 10;
  a += s.x;
  return a;
}
