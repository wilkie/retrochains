struct R { int a; int b; int c; };
int sum(struct R r) {
  return r.a + r.b + r.c;
}
int main(void) {
  struct R q;
  q.a = 1;
  q.b = 2;
  q.c = 3;
  return sum(q);
}
