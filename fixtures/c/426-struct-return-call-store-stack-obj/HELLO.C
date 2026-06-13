struct S { int x; int y; };
struct S f(void);
int main(void) {
  struct S a;
  a = f();
  return 0;
}
