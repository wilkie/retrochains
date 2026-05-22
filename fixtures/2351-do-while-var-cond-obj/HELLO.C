int main(void) {
  int i;
  int n;
  int sum;
  i = 0;
  n = 5;
  sum = 0;
  do {
    sum = sum + i;
    i = i + 1;
  } while (i < n);
  return sum;
}
