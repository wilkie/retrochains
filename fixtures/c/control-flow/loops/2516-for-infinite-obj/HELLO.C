int g;
int main(void) {
  for (;;) {
    g = g + 1;
    if (g > 100) break;
  }
  return g;
}
