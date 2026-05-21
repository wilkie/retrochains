int apply(int (*f)(int), int x) { return f(x); }
int sq(int n) { return n * n; }
int main(void) {
  return apply(sq, 6);
}
