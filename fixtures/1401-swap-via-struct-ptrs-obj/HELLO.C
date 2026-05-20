struct S { int x; int y; };
struct S s1;
struct S s2;
void swap(struct S *a, struct S *b) {
  int t;
  t = a->x;
  a->x = b->x;
  b->x = t;
}
int main(void) {
  s1.x = 5;
  s2.x = 7;
  swap(&s1, &s2);
  return s1.x;
}
