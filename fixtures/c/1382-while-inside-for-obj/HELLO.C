int main(void) {
  int s = 0;
  int i;
  int j;
  for (i = 0; i < 3; i++) {
    j = i;
    while (j > 0) {
      s++;
      j--;
    }
  }
  return s;
}
