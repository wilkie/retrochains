int main(void) {
  int i = 0;
  int sum = 0;
  do {
    sum += i;
    i++;
    if (sum > 5) break;
  } while (i < 10);
  return sum;
}
