int sum2x2(int a[2][2]) {
  return a[0][0] + a[0][1] + a[1][0] + a[1][1];
}
int main(void) {
  int m[2][2];
  m[0][0] = 1; m[0][1] = 2;
  m[1][0] = 3; m[1][1] = 4;
  return sum2x2(m);
}
