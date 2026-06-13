int main(void) {
  int i, j, sum;
  sum = 0;
  for (i = 0, j = 10; i < j; i++, j--) {
    sum = sum + i;
  }
  return sum;
}
