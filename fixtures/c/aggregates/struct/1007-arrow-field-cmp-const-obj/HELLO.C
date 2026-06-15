struct S { int x; };
int main(void) {
  struct S a;
  struct S *p;
  p = &a;
  p->x = 5;
  if (p->x == 5) return 7;
  return 0;
}
