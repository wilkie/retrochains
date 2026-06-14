int main(void) {
  int x;
  int huge *p1 = (int huge *)&x;
  int huge *p2 = (int huge *)&x;
  int huge *p3 = (int huge *)&x;
  if (p1 != p3) return 5;
  return 7;
}
