int main(void) {
  int i;
  int sum;
  i = 5;
  sum = 0;
  do {
    sum = sum + i;
    i = i - 1;
  } while (i > 0);
  return sum;
}
