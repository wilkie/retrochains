void compute(int n, int *r) {
  *r = n * n + 1;
}
int main(void) {
  int x;
  compute(5, &x);
  return x;
}
