int main(void) {
  int i = 0;
  int sum = 0;
loop:
  if (i >= 5) goto done;
  sum += i;
  i++;
  goto loop;
done:
  return sum;
}
