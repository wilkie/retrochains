int main(void) {
  int i = 0;
  int sum = 0;
  while (i < 10) {
    ++i;
    if (i == 5) continue;
    sum = sum + i;
  }
  return sum;
}
