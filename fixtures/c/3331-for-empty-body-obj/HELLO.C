int find(int *p, int n) {
  int i;
  for (i = 0; i < n && p[i] != 0; i++)
    ;
  return i;
}
