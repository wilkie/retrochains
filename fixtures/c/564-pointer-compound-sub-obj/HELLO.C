int main(void) {
  int a[5];
  int *p;
  p = a;
  p += 4;
  p -= 2;
  *p = 7;
  return 0;
}
