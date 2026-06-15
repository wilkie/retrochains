int main(void) {
  int a[100];
  a[0] = 1;
  a[50] = 50;
  a[99] = 99;
  return a[0] + a[50] + a[99];
}
