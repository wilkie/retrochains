struct T { int a; int b; int c; };
int sum_t(struct T t) {
  return t.a + t.b + t.c;
}
int main(void) {
  struct T s;
  s.a = 1;
  s.b = 2;
  s.c = 3;
  return sum_t(s);
}
