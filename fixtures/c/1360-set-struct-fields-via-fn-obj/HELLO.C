struct S { int x; int y; };
struct S s;
void setBoth(struct S *p, int a, int b) {
  p->x = a;
  p->y = b;
}
int main(void) {
  setBoth(&s, 3, 4);
  return s.x + s.y;
}
