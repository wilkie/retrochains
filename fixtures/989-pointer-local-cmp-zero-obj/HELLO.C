int g;
int main(void) {
  int *p;
  p = &g;
  if (p == 0) return 1;
  return 0;
}
