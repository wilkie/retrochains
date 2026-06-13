struct Pair { int a; int b; };
struct Pair make(void);
int main(void) {
  struct Pair p;
  p = make();
  return p.a + p.b;
}
