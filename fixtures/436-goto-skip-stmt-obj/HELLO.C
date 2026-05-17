int g;
int main(void) {
  if (g) goto skip;
  g = 1;
skip:
  return 0;
}
