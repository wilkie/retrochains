struct S { long x; };
int f(struct S *p) {
  p->x = 7;
  return 0;
}
int main(void) {
  return 0;
}
