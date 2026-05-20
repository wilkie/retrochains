int main(void) {
  int a[2];
  int t;
  a[0] = 5;
  a[1] = 10;
  t = a[0];
  a[0] = a[1];
  a[1] = t;
  return a[0];
}
