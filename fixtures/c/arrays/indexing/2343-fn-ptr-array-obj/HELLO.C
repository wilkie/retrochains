int add1(int x) { return x + 1; }
int add2(int x) { return x + 2; }
int add3(int x) { return x + 3; }
int main(void) {
  int (*fns[3])(int);
  fns[0] = add1;
  fns[1] = add2;
  fns[2] = add3;
  return fns[0](10) + fns[1](20) + fns[2](30);
}
