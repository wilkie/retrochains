void f(int *r) {
  if (*r == 0) return;
  *r = 1;
}
int main(void) {
  int x = 5;
  f(&x);
  return x;
}
