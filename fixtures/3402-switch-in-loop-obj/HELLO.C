int count(int *p, int n) {
  int c = 0;
  int i;
  for (i = 0; i < n; i++) {
    switch (p[i]) {
      case 1: c++; break;
      case 2: c += 2; break;
    }
  }
  return c;
}
