int main(void) {
  int x;
  int huge *p1 = (int huge *)&x;
  int huge *p2 = (int huge *)&x;
  if (p1 == p2) return 1;
  return 0;
}
