int main(void) {
  int i = 0;
  int j;
  int s = 0;
  while (i < 2) {
    j = 0;
    while (j < 2) {
      s++;
      j++;
    }
    i++;
  }
  return s;
}
