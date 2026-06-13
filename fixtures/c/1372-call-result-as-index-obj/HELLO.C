int idx(void) {
  return 1;
}
int main(void) {
  int a[3];
  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  return a[idx()];
}
