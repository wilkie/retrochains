int g = 100;
int main(void) {
  int g = 5;
  {
    int g = 1;
    g = g + 10;
  }
  return g;
}
