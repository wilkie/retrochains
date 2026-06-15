int main(void) {
  int i;
  int sum;
  i = 0;
  sum = 0;
  do {
    i = i + 1;
    if (i == 4) continue;
    if (i == 7) break;
    sum = sum + i;
  } while (i < 10);
  return sum;
}
