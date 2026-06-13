int g;
int main(void) {
  int x;
  if (g) goto skip;
  x = g + 1;
  g = x;
skip:
  return 0;
}
