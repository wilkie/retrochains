int main(void) {
  int i = 0;
  int sum = 0;
  do {
    ++i;
    if (i == 3) continue;
    sum = sum + i;
  } while (i < 5);
  return sum;
}
