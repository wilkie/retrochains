struct Pair { int a; int b; };
struct Pair make(void) {
  struct Pair p;
  p.a = 100;
  p.b = 200;
  return p;
}
int main(void) {
  struct Pair x;
  x = make();
  return x.a + x.b;
}
