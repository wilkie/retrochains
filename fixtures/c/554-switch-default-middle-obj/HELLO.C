int main(void) {
  int x;
  int r;
  x = 5;
  r = 0;
  switch (x) {
    case 1: r = 10; break;
    default: r = 99; break;
    case 2: r = 20; break;
  }
  return r;
}
