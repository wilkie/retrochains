int main(void) {
  int i;
  int sum;
  i = 0;
  sum = 0;
  do {
    i = i + 1;
    if (i == 3) continue;
    if (sum > 8) break;
    sum = sum + i;
  } while (i < 9);
  return sum;
}
