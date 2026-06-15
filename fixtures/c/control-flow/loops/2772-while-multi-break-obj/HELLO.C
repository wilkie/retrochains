int main(void) {
  int i;
  i = 0;
  while (1) {
    if (i == 3) break;
    i = i + 1;
    if (i == 5) break;
  }
  return i;
}
