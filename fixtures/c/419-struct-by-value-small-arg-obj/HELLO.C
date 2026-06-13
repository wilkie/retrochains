struct S { int x; int y; };
int f(struct S s) { return 0; }
struct S g;
int main(void) {
  f(g);
  return 0;
}
