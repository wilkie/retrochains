struct S { long x; };
struct S s;
int main(void) {
  struct S *p = &s;
  p->x += 5;
  return 0;
}
