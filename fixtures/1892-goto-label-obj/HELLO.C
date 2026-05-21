int main(void) {
  int i = 0;
  int sum = 0;
loop:
  sum += i;
  i++;
  if (i < 5) goto loop;
  return sum;
}
