struct S { int x; int y; int z; };
struct S a;
struct S b;
int main(void) {
  a = b;
  return 0;
}
