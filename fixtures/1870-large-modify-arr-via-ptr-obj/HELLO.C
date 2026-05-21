void fill(int *a) {
  a[0] = 10;
  a[1] = 20;
}
int main(void) {
  int x[2];
  x[0] = 1;
  x[1] = 2;
  fill(x);
  return x[0] + x[1];
}
