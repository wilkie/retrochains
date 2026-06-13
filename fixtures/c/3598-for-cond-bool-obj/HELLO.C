int *find(int *p, int n, int v) {
  while (n-- && *p != v) p++;
  return p;
}
