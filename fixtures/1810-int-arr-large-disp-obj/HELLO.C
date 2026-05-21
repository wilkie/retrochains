int main(void) {
  int a[80];
  a[0] = 1;
  a[70] = 99;
  return a[0] + a[70];
}
