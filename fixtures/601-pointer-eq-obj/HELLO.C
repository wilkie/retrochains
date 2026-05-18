int g;
int main(void) {
  int *p;
  int *q;
  p = &g;
  q = &g;
  if (p == q) return 1;
  return 0;
}
