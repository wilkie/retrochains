int main(void) {
  int i;
  int j;
  int sum;
  sum = 0;
  for (i = 0, j = 10; i < 5; i = i + 1, j = j - 1) {
    sum = sum + i + j;
  }
  return sum;
}
