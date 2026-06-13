int main(void) {
  int x;
  int y;
  x = 1;
  y = 2;
  switch (x) {
    case 1:
      switch (y) {
        case 2: return 12;
      }
      return 10;
    case 2: return 20;
  }
  return 0;
}
