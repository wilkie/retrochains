char arr[10];

int sum(void) {
  int s, i;
  char c;
  s = 0;
  i = 0;
  while ((c = arr[i++]) != 0) s += c;
  return s;
}
