long g;
int main(void) {
  long *p = &g;
  p[0] = 42;
  return 0;
}
