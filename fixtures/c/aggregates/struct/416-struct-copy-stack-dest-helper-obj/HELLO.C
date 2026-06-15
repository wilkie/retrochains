struct S { int x; int y; int z; };
struct S b;
int main(void) {
  struct S a;
  a = b;
  return 0;
}
