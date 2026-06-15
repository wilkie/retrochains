int main(void) {
  int a[3];
  int huge *p1 = (int huge *)&a[0];
  int huge *p2 = (int huge *)&a[2];
  return (int)(p2 - p1);
}
