int main(void) {
  int i = 0;
  int j;
  while (i < 5) {
    j = 0;
    while (j < 5) {
      if (j == 2) break;
      ++j;
    }
    ++i;
  }
  return i;
}
