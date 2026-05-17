int g;
int main(void) {
  int i;
  for (i = 0; i < 10; i = i + 1) {
    if (i == 3) goto found;
    g = g + i;
  }
found:
  return 0;
}
