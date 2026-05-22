int seek(int n) {
  int i;
  i = 0;
  while (i < n) {
    if (i == 5) break;
    i = i + 1;
  }
  return i;
}
