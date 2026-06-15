int main(void) {
  int i = 0;
  int s = 0;
top:
  if (i >= 5) goto done;
  s += i;
  i++;
  goto top;
done:
  return s;
}
