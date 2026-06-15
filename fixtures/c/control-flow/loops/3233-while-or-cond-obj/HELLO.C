int data[10];
int scan(int n) {
  int i;
  i = 0;
  while (i < n || data[0] != 0) {
    i = i + 1;
    if (i > 100) break;
  }
  return i;
}
