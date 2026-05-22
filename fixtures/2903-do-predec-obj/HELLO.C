int count(int n) {
  int s;
  s = 0;
  do {
    s = s + 1;
  } while (--n);
  return s;
}
