struct S { int x; };
struct S s = {3};
int a[2] = {5, 7};
int main(void) {
  return s.x + a[1];
}
