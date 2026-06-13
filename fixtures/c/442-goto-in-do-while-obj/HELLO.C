int g;
int main(void) {
  do {
    if (g == 5) goto done;
    g = g + 1;
  } while (g < 10);
done:
  return 0;
}
