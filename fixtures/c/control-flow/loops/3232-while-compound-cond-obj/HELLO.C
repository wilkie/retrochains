int data[10];
int scan(int n) {
  int i;
  i = 0;
  while (i < n && data[i] != 0) {
    i = i + 1;
  }
  return i;
}
