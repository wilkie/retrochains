int main(void) {
  int i;
  int sum;
  i = 0;
  sum = 0;
top:
  sum = sum + i;
  i = i + 1;
  if (i < 5) goto top;
  return sum;
}
