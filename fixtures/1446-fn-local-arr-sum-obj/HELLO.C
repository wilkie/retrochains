int sum_local(void) {
  int a[3];
  a[0] = 1;
  a[1] = 2;
  a[2] = 3;
  return a[0] + a[1] + a[2];
}
int main(void) {
  return sum_local();
}
