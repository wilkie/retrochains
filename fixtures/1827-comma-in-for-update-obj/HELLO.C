int main(void) {
  int i, j;
  int sum = 0;
  for (i = 0, j = 10; i < 5; i++, j--) {
    sum += i + j;
  }
  return sum;
}
