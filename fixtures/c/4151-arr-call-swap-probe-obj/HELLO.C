int dbl(int x) { return x * 2; }
int main(void) {
  int a[3];
  a[0] = 1; a[1] = 2; a[2] = 3;
  return dbl(a[2]) + dbl(a[1]);
}
