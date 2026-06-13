int main(void) {
  int i;
  i = 0;
top:
  i = i + 1;
  if (i < 3) goto top;
  return i;
}
