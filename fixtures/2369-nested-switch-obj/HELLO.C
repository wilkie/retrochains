int main(void) {
  int x;
  int y;
  int r;
  x = 1;
  y = 2;
  r = 0;
  switch (x) {
    case 1:
      switch (y) {
        case 1: r = 11; break;
        case 2: r = 12; break;
      }
      break;
    case 2:
      r = 99;
      break;
  }
  return r;
}
