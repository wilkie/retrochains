int main(void) {
  int i;
  int sum;
  sum = 0;
  for (i = 1; i <= 10; i++) {
    if (i > 5) break;
    sum = sum + i;
  }
  return sum;
}
