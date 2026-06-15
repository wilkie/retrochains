int g;
int main(void) {
top:
  g = g + 1;
  if (g < 3) goto top;
  return 0;
}
