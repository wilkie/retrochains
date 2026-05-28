int printf(char *, ...);
int sum_to(int n) {
  int i;
  int total = 0;
  for (i = 1; i <= n; i = i + 1) {
    total = total + i;
  }
  return total;
}
int main(void) {
  printf("sum=%d\n", sum_to(10));
  return 0;
}
