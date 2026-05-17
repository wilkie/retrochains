struct S { int x; int y; };
struct S f(void);
struct S a;
int main(void) {
  a = f();
  return 0;
}
