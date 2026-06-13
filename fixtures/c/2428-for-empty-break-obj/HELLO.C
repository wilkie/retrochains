int main(void) {
  int i;
  i = 0;
  for (;;) {
    if (i > 3) break;
    i = i + 1;
  }
  return i;
}
