int sum_three(int a[]) {
  return a[0] + a[1] + a[2];
}
int main(void) {
  int x[3];
  x[0] = 10;
  x[1] = 20;
  x[2] = 30;
  return sum_three(x);
}
