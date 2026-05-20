int sum(int *p) {
  return p[0] + p[1];
}
int main(void) {
  int a[2];
  a[0] = 3;
  a[1] = 4;
  return sum(a);
}
