int sum(int n) {
  int s = 0;
  int i, j;
  for (i = 0; i < n; i++)
    for (j = 0; j < n; j++)
      s += i * j;
  return s;
}
