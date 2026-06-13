int first(int *p) { return p[0]; }
int main(void) {
  int a[3];
  a[0] = 42;
  a[1] = 99;
  a[2] = 100;
  return first(a);
}
