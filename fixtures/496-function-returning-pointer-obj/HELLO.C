int g;
int *f(void) {
  return &g;
}
int main(void) {
  int *p;
  p = f();
  *p = 7;
  return 0;
}
