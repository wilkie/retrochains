int main(void) {
  char a[4];
  char b[4];
  int i;
  int diff;
  a[0] = 'X'; a[1] = 'Y'; a[2] = 'Z'; a[3] = 0;
  b[0] = 'X'; b[1] = 'Q'; b[2] = 'Z'; b[3] = 0;
  diff = 0;
  for (i = 0; a[i] != 0; i = i + 1) {
    if (a[i] != b[i]) {
      diff = a[i] - b[i];
      i = i + 100;
    }
  }
  return diff;
}
