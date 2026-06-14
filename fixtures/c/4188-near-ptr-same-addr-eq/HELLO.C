int main(void) {
  int x;
  int *p1 = &x;
  int *p2 = &x;
  if (p1 == p2) return 1;
  return 0;
}
