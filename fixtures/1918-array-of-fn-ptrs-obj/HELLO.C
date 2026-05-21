int add1(int x) { return x + 1; }
int add2(int x) { return x + 2; }
int main(void) {
  int (*fns[2])(int);
  fns[0] = add1;
  fns[1] = add2;
  return fns[0](10) + fns[1](20);
}
