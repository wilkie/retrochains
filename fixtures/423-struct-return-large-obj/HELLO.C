struct S { int x; int y; int z; };
struct S g;
struct S f(void) {
  return g;
}
int main(void) {
  return 0;
}
