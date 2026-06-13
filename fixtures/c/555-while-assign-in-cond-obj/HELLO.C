int g;
int main(void) {
  int c;
  g = 3;
  while ((c = g) > 0) {
    g = g - 1;
  }
  return c;
}
