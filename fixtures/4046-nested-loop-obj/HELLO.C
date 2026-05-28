int main(void) {
  int i, j;
  int sum = 0;
  for (i = 0; i < 3; i = i + 1) {
    for (j = 0; j < 3; j = j + 1) {
      sum = sum + 1;
    }
  }
  return sum;
}
