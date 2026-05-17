int main(void) {
  int i;
  int j;
  j = 100;
  for (i = 0; i < 3; i = i + 1, j = j - 1) {
    if (j == 0) return -1;
  }
  return j;
}
