struct Outer {
  int a;
  struct {
    int b;
    int c;
  } inner;
} s = {1, {2, 3}};

int sum(void) {
  return s.a + s.inner.b + s.inner.c;
}
