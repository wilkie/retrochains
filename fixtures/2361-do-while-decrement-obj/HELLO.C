int main(void) {
  int n;
  int sum;
  n = 5;
  sum = 0;
  do {
    sum = sum + n;
  } while (--n);
  return sum;
}
