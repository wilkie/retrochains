struct S { int x; int y; };
struct S b;
int main(void) {
  struct S a;
  a = b;
  return 0;
}
