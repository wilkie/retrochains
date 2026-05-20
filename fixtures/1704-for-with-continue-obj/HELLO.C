int main(void) {
  int i;
  int sum = 0;
  for (i = 0; i < 10; i++) {
    if (i & 1) continue;
    sum += i;
  }
  return sum;
}
