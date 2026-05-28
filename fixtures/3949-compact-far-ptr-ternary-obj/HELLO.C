int g1, g2;
int main(void) {
  int *p = &g1;
  int *q = &g2;
  return p == q ? 1 : 0;
}
