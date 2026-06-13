int main(void) {
  int a[3];
  int v;
  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  v = a[1]++;
  return a[1] * 10 + v;
}
