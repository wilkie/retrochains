int main(void) {
  int i;
  int sum;
  i = 0;
  sum = 0;
  goto check;
body:
  sum = sum + i;
  i = i + 1;
check:
  if (i < 5) goto body;
  return sum;
}
