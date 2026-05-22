int main(void) {
  int x;
  int r;
  x = 1;
  r = 0;
  switch (x) {
    default: r = 99; break;
    case 1: r = 11; break;
    case 2: r = 22; break;
  }
  return r;
}
