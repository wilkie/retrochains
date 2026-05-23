int meet(int n) {
  int i, j;
  for (i = 0, j = n; i < j; i++, j--)
    ;
  return i;
}
