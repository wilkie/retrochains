struct S { long x; };
struct S s;
int main(void) {
  struct S *p = &s;
  p->x++;
  return 0;
}
