int g;
int main(void) {
  int *p;
  p = &g;
  p[0] = 42;
  return g;
}
