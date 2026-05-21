enum Color { RED = 1, GREEN = 2, BLUE = 4 };
int main(void) {
  enum Color c = GREEN;
  int n = c + RED;
  return n;
}
