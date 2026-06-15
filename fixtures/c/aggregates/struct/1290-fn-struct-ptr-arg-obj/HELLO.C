struct S { int x; };
struct S s;
void inc(struct S *p) {
  p->x++;
}
int main(void) {
  s.x = 5;
  inc(&s);
  return s.x;
}
