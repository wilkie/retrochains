enum Color { RED, GREEN, BLUE };
int main(void) {
  enum Color c;
  int r;
  c = GREEN;
  r = 0;
  switch (c) {
    case RED: r = 100; break;
    case GREEN: r = 200; break;
    case BLUE: r = 300; break;
  }
  return r;
}
