int main(void) {
  int x;
  int r;
  x = 10;
  r = 0;
  switch (x) {
    case 1: r = 100; break;
    case 5: r = 200; break;
    case 10: r = 300; break;
    case 100: r = 400; break;
  }
  return r;
}
