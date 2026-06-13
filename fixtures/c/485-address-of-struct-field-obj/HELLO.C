struct S { int x; int y; };
struct S s;
int *p;
int main(void) {
  p = &s.y;
  return 0;
}
