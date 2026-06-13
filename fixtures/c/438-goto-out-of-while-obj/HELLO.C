int g;
int main(void) {
  while (g < 10) {
    if (g == 5) goto done;
    g = g + 1;
  }
done:
  return 0;
}
