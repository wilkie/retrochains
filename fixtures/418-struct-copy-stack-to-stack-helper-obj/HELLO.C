struct S { int x; int y; int z; };
int main(void) {
  struct S a;
  struct S b;
  a = b;
  return 0;
}
