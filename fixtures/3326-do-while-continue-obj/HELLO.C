int count(int n) {
  int s = 0;
  do {
    if (n & 1) {
      n >>= 1;
      continue;
    }
    s++;
    n >>= 1;
  } while (n);
  return s;
}
