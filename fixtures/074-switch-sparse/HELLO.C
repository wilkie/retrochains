int main(void) {
  int x = 100;
  int r = 0;
  switch (x) {
    case 1: r = 10; break;
    case 10: r = 20; break;
    case 100: r = 30; break;
    case 1000: r = 40; break;
  }
  return r;
}
