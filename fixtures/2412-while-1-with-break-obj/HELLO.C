int main(void) {
  int i;
  int sum;
  i = 0;
  sum = 0;
  while (1) {
    if (i > 5) break;
    sum = sum + i;
    i = i + 1;
  }
  return sum;
}
