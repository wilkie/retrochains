int g;
int main(void) {
  int *p;
  g = 42;
  p = &g;
  return p[0];
}
