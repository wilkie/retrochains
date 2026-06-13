int main(void) {
  int i;
  int sum;
  i = 0;
  sum = 0;
  do {
    i = i + 1;
    if (i == 3) continue;
    sum = sum + i;
  } while (i < 5);
  return sum;
}
